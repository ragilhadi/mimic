use crate::types::{create_mock_key, MockConfig, MockStore};
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

/// One mock file — or one element of an array file — that failed to load,
/// with the reason it failed.
#[derive(Debug, Clone)]
pub struct LoadError {
    /// The file the failure is in, as the walk named it.
    pub file: PathBuf,
    /// `Some(index)` when the file is an array of mocks and only element
    /// `index` failed validation — its siblings still load. `None` when the
    /// whole file failed: unreadable, or not valid JSON/YAML.
    pub element: Option<usize>,
    /// The reason, naming the file (or `file#index`) it is about.
    pub message: String,
}

impl LoadError {
    /// What to name this failure as in diagnostics: the file, or the
    /// positional `file#index` for an element error.
    pub fn target(&self) -> String {
        match self.element {
            Some(index) => format!("{}#{}", self.file.display(), index),
            None => self.file.display().to_string(),
        }
    }
}

/// Result of loading mock configurations, including any errors encountered.
pub struct LoadResult {
    pub mocks: HashMap<String, Vec<MockConfig>>,
    /// Per-file (or per-element) failures — the target and *why* it failed —
    /// in the walk's sorted order. The loader only ever had a count of
    /// these; carrying the messages is what lets a consumer log or report
    /// them without re-deriving them (the hot-reload summary,
    /// `mimic validate`), and carrying the element is what lets a hot reload
    /// carry forward exactly the mock that failed and not its siblings.
    pub errors: Vec<LoadError>,
    /// How many candidate mock files the walk scanned: everything counted in
    /// [`Self::errors`] and every mock in [`Self::mocks`], plus files skipped
    /// as fixtures. The number a validator's summary line talks about.
    pub files: usize,
}

// ============================================================================
// Hot-reload change detection (#110)
// ============================================================================

/// A cheap stand-in for "have this file's bytes changed since I last read
/// it?" — its modified time and length, both already free with every `stat`
/// call the walk makes anyway.
///
/// Not foolproof (a same-second edit that happens to leave the length
/// unchanged is a known gap of mtime+size schemes generally), but it's
/// dependency-free and turns an unconditional re-read into a re-read only
/// when something plausibly changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    modified: std::time::SystemTime,
    len: u64,
}

impl FileFingerprint {
    fn of(metadata: &fs::Metadata) -> Option<Self> {
        Some(Self {
            modified: metadata.modified().ok()?,
            len: metadata.len(),
        })
    }
}

/// A mock file's last successful parse, plus the fingerprints of every
/// fixture it depends on — so a `response_file` edit is detected even though
/// the mock file that references it hasn't changed.
///
/// "Successful" means *every* element of the file loaded: an element that
/// fails validation would otherwise freeze that verdict under an unchanged
/// fingerprint, and fixing it would never take effect.
struct CachedMock {
    fingerprint: FileFingerprint,
    fixture_fingerprints: Vec<(PathBuf, FileFingerprint)>,
    outcome: LoadedFile,
}

/// Cross-cycle state a hot-reload loop carries so it can skip re-reading and
/// re-parsing whatever hasn't changed.
///
/// Owned by the caller (the reload task in `main.rs`) and threaded through
/// [`load_mocks_map_hot_reload`] on every cycle. A fresh, empty cache makes
/// every file a cache miss — which is exactly [`load_mocks_map`]'s always-read
/// behavior, so that function is implemented as "call the cached path with a
/// cache nobody keeps."
#[derive(Default)]
pub struct LoaderCache {
    files: HashMap<PathBuf, CachedMock>,
    /// Fixture bytes, keyed by canonical path rather than by the mock that
    /// references them — so two mocks sharing a fixture, or the same mock
    /// file across cycles, reuse one read.
    fixtures: HashMap<PathBuf, (FileFingerprint, Bytes)>,
}

impl LoaderCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A hot-reload cycle's outcome: the loaded mock set, and whether anything
/// actually changed since the cache's last cycle.
///
/// `changed` is what lets the caller skip taking the mock store's write lock
/// — and re-running the shadowed-mock check and sequence-state pruning that
/// follow it — on a cycle where nothing did.
pub struct ReloadOutcome {
    pub result: LoadResult,
    pub changed: bool,
}

// ============================================================================
// response_file limits
// ============================================================================

/// Default cap on a single `response_file`, in bytes (10 MB).
pub const DEFAULT_MAX_RESPONSE_FILE: u64 = 10 * 1024 * 1024;

/// Environment variable overriding [`DEFAULT_MAX_RESPONSE_FILE`], in bytes.
pub const MAX_RESPONSE_FILE_ENV: &str = "MIMIC_MAX_RESPONSE_FILE";

/// The largest `response_file` this process will load.
///
/// Every fixture is held in memory for as long as it's registered and copied
/// into the mock map on every reload cycle, so an unbounded fixture is a
/// footgun pointed at a server whose whole pitch is a 10 ms response. `0`
/// disables the cap for a run that genuinely wants a huge fixture.
pub fn max_response_file_size() -> u64 {
    static MAX: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| match std::env::var(MAX_RESPONSE_FILE_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                warn!(
                    "Invalid {}='{}', falling back to {} bytes",
                    MAX_RESPONSE_FILE_ENV, raw, DEFAULT_MAX_RESPONSE_FILE
                );
                DEFAULT_MAX_RESPONSE_FILE
            }
        },
        Err(_) => DEFAULT_MAX_RESPONSE_FILE,
    })
}

/// One mock as it came off disk: the config, plus the fixture files it claims.
///
/// The fixture list is what lets the directory walk tell a fixture apart from
/// a mock. Fixtures live inside the mocks root — the containment check requires
/// it — so a `.json` fixture would otherwise be picked up by the walk and
/// reported as a mock file that failed to parse.
#[derive(Clone)]
struct LoadedMock {
    mock: MockConfig,
    /// Canonical paths of the files this mock serves its bodies from.
    fixtures: Vec<PathBuf>,
}

/// One mock-candidate file's outcome: the mocks it holds, loaded or failed
/// in place, or a whole-file failure that never got as far as any of them.
#[derive(Clone)]
enum LoadedFile {
    /// The file couldn't be read or parsed at all — nothing is known about
    /// any mock inside it, so the file fails as a whole, as it always has.
    Failed(String),
    /// The file parsed, holding one outcome per mock it contains, in file
    /// order. `array` records the file's top-level shape, which decides how
    /// its failures are reported: a single-object file fails as a whole
    /// (no element index, byte-for-byte as it always did), while an element
    /// of an array file fails in place as `file#index` without stopping its
    /// siblings.
    Parsed {
        array: bool,
        outcomes: Vec<Result<LoadedMock, String>>,
    },
}

impl LoadedFile {
    /// Every fixture file the loaded mocks of this file claim.
    fn fixtures(&self) -> Vec<&PathBuf> {
        match self {
            LoadedFile::Parsed { outcomes, .. } => outcomes
                .iter()
                .filter_map(|outcome| outcome.as_ref().ok())
                .flat_map(|loaded| loaded.fixtures.iter())
                .collect(),
            LoadedFile::Failed(_) => Vec::new(),
        }
    }

    /// Whether every mock of this file loaded — the condition under which an
    /// outcome may enter the hot-reload cache.
    fn fully_loaded(&self) -> bool {
        match self {
            LoadedFile::Parsed { outcomes, .. } => outcomes.iter().all(Result::is_ok),
            LoadedFile::Failed(_) => false,
        }
    }

    /// How the file's `index`th mock reports its element: `Some` inside an
    /// array file, `None` in a single-object one.
    fn element_of(array: bool, index: usize) -> Option<usize> {
        array.then_some(index)
    }
}

// ============================================================================
// Mocks directory resolution
// ============================================================================

/// Environment variable naming the directory (or single file) mocks are read
/// from, overriding the defaults below.
pub const MOCKS_DIR_ENV: &str = "MIMIC_MOCKS_DIR";

/// Where the Docker image mounts mocks. Probed first when the variable is
/// unset, so every existing `docker run -v ./mocks:/app/mocks` keeps resolving
/// exactly where it always did.
pub const DOCKER_MOCKS_DIR: &str = "/app/mocks";

/// Where a local run keeps its mocks — the directory the README has always
/// said Mimic reads, and the parent of the importer's default output.
pub const LOCAL_MOCKS_DIR: &str = "./mocks";

/// How [`resolve_mocks_dir`] arrived at a directory.
///
/// Carried alongside the path so startup can say *why* it is reading where it
/// is reading: "loaded 0 mocks" is a very different problem depending on
/// whether the operator named the directory or Mimic guessed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MocksDirOrigin {
    /// `MIMIC_MOCKS_DIR` was set to a non-empty value.
    Configured,
    /// Defaulted to the Docker mount point, which exists.
    Docker,
    /// Defaulted to `./mocks`, relative to the working directory.
    Local,
}

impl MocksDirOrigin {
    /// A short phrase for the startup log.
    pub fn describe(self) -> &'static str {
        match self {
            MocksDirOrigin::Configured => "from MIMIC_MOCKS_DIR",
            MocksDirOrigin::Docker => "default, Docker mount point",
            MocksDirOrigin::Local => "default, relative to the working directory",
        }
    }
}

/// The mocks directory a run will read, and how it was chosen.
#[derive(Debug, Clone)]
pub struct ResolvedMocksDir {
    pub path: String,
    pub origin: MocksDirOrigin,
    /// Whether the resolved path exists at resolution time. Only a snapshot —
    /// hot reload re-checks every cycle, so a directory created after startup
    /// still gets picked up.
    pub exists: bool,
}

/// Resolve the mocks directory from the environment and the filesystem.
///
/// `MIMIC_MOCKS_DIR` wins outright and is used verbatim, present or not: an
/// operator who named a directory wants to be told it's missing, not to have
/// Mimic quietly read a different one. With the variable unset, `/app/mocks`
/// is used when it exists — that's the Docker image — and `./mocks` otherwise.
pub fn resolve_mocks_dir() -> ResolvedMocksDir {
    resolve_mocks_dir_from(std::env::var(MOCKS_DIR_ENV).ok(), |path| {
        Path::new(path).exists()
    })
}

/// [`resolve_mocks_dir`] with the environment and the filesystem injected.
///
/// Split out so the resolution rule is testable without mutating a
/// process-wide environment variable that every other test shares.
pub fn resolve_mocks_dir_from(
    configured: Option<String>,
    path_exists: impl Fn(&str) -> bool,
) -> ResolvedMocksDir {
    if let Some(raw) = configured {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return ResolvedMocksDir {
                exists: path_exists(trimmed),
                path: trimmed.to_string(),
                origin: MocksDirOrigin::Configured,
            };
        }
    }

    if path_exists(DOCKER_MOCKS_DIR) {
        return ResolvedMocksDir {
            path: DOCKER_MOCKS_DIR.to_string(),
            origin: MocksDirOrigin::Docker,
            exists: true,
        };
    }

    ResolvedMocksDir {
        exists: path_exists(LOCAL_MOCKS_DIR),
        path: LOCAL_MOCKS_DIR.to_string(),
        origin: MocksDirOrigin::Local,
    }
}

/// Loads mock configurations from a directory or file into a raw HashMap.
///
/// Args:
///     path (str): Path to a directory containing `.json`/`.yaml`/`.yml` mock
///     files, or a single such file.
///
/// Returns:
///     LoadResult containing mock configurations keyed by "METHOD:PATH" and the
///     number of files that failed to load.
pub fn load_mocks_map(path: &str) -> LoadResult {
    load_mocks_map_with_limit(path, max_response_file_size())
}

/// [`load_mocks_map`] with the `response_file` size cap injected, so the cap
/// can be exercised without a process-wide environment variable.
///
/// Implemented on top of the same cached walk [`load_mocks_map_hot_reload`]
/// uses: a cache nobody keeps between calls is empty on every call, which
/// makes every file a miss and every file get (re-)read — this function's
/// contract, unchanged.
pub fn load_mocks_map_with_limit(path: &str, max_response_file: u64) -> LoadResult {
    let mut cache = LoaderCache::new();
    load_mocks_map_core(path, max_response_file, &mut cache).result
}

/// Reload for a long-lived hot-reload loop: `cache` carries fingerprints and
/// fixture bytes forward from the previous cycle, so a file whose `stat`
/// hasn't changed is neither re-read nor re-parsed, and [`ReloadOutcome::changed`]
/// tells the caller whether the mock store needs updating at all.
///
/// Uses the process-wide [`max_response_file_size`] cap, matching
/// [`load_mocks_map`].
pub fn load_mocks_map_hot_reload(path: &str, cache: &mut LoaderCache) -> ReloadOutcome {
    load_mocks_map_core(path, max_response_file_size(), cache)
}

/// The walk both [`load_mocks_map_with_limit`] and [`load_mocks_map_hot_reload`]
/// share; only whether `cache` survives past this one call differs between them.
fn load_mocks_map_core(
    path: &str,
    max_response_file: u64,
    cache: &mut LoaderCache,
) -> ReloadOutcome {
    let path_obj = Path::new(path);

    if !path_obj.exists() {
        warn!("Mock path does not exist: {}", path);
        let was_populated = !cache.files.is_empty();
        cache.files.clear();
        return ReloadOutcome {
            result: LoadResult {
                mocks: HashMap::new(),
                errors: vec![LoadError {
                    file: PathBuf::from(path),
                    element: None,
                    message: "mock path does not exist".to_string(),
                }],
                files: 0,
            },
            changed: was_populated,
        };
    }

    // The root a `response_file` may not escape: the mocks directory itself,
    // or — for a single-file load — the directory that file lives in.
    let root = if path_obj.is_dir() {
        path_obj.to_path_buf()
    } else {
        path_obj.parent().unwrap_or(Path::new(".")).to_path_buf()
    };
    let root = root.canonicalize().unwrap_or(root);

    // Pass one: read every candidate file, in a fixed order — skipping
    // whatever `cache` says is unchanged since last time.
    let mut scanned: Vec<(PathBuf, LoadedFile)> = Vec::new();
    let mut errors: Vec<LoadError> = Vec::new();
    let mut changed = false;
    // Every mock-candidate file seen this cycle, so a file that vanished
    // (deleted or renamed away) can be dropped from `cache` afterward instead
    // of lingering there forever.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    if path_obj.is_file() {
        seen.insert(path_obj.to_path_buf());
        let outcome =
            load_single_mock_maybe_cached(path_obj, &root, max_response_file, cache, &mut changed);
        scanned.push((path_obj.to_path_buf(), outcome));
    } else if path_obj.is_dir() {
        // Seeded with the root itself: a symlink that resolves back to the
        // directory the walk started from (`mocks/sub/loop -> ..`) is a cycle
        // even though the walk hasn't "visited" it as a subdirectory yet.
        let mut visited = HashSet::new();
        visited.insert(root.clone());
        let mut state = ScanState {
            scanned: &mut scanned,
            errors: &mut errors,
            visited: &mut visited,
            cache,
            changed: &mut changed,
            seen: &mut seen,
        };
        collect_mock_files(path_obj, &root, max_response_file, &mut state, 0);
    }

    let before = cache.files.len();
    cache.files.retain(|file, _| seen.contains(file));
    if cache.files.len() != before {
        changed = true;
    }

    // Every fixture claimed as a fixture by a mock that loaded. A fixture is
    // served, never registered — including when it is itself valid JSON. The
    // same set also prunes cached fixture bytes no mock claims any more:
    // `cache.files` is pruned against `seen` above; the fixture map is keyed
    // by fixture path, so a deleted or renamed fixture would otherwise stay
    // in memory for the life of the process.
    let claimed: HashSet<&PathBuf> = scanned.iter().flat_map(|(_, f)| f.fixtures()).collect();
    cache.fixtures.retain(|path, _| claimed.contains(path));

    let fixtures: HashSet<&PathBuf> = scanned.iter().flat_map(|(_, f)| f.fixtures()).collect();

    // Pass two: register what parsed, minus the fixtures.
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    let mut fixture_files: usize = 0;
    for (file, outcome) in &scanned {
        // Only worth a `canonicalize` syscall per file when some mock actually
        // claims a fixture, which for most mock sets is never.
        let canonical = (!fixtures.is_empty())
            .then(|| file.canonicalize().ok())
            .flatten();
        if canonical.is_some_and(|path| fixtures.contains(&path)) {
            debug!(
                "Skipping {}: it is served as a response_file by another mock",
                file.display()
            );
            fixture_files += 1;
            continue;
        }

        match outcome {
            LoadedFile::Failed(message) => {
                errors.push(LoadError {
                    file: file.clone(),
                    element: None,
                    message: message.clone(),
                });
            }
            LoadedFile::Parsed { array, outcomes } => {
                for (index, element) in outcomes.iter().enumerate() {
                    match element {
                        Ok(loaded) => {
                            register_mock(
                                &mut mocks,
                                file,
                                LoadedFile::element_of(*array, index),
                                loaded,
                            );
                        }
                        Err(message) => {
                            errors.push(LoadError {
                                file: file.clone(),
                                // A single-object file's failure is the whole
                                // file's — no element index — so the hot
                                // reload carries its last-known-good mock the
                                // way it always has.
                                element: LoadedFile::element_of(*array, index),
                                message: message.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    // A file that fails to parse isn't cached (see
    // `load_single_mock_maybe_cached`), so it's retried — and `errors`
    // stays nonempty — every cycle until it's fixed or removed. Treating
    // that as "changed" every time matches `apply_reload`'s existing
    // carry-forward behavior, which already re-runs on every such cycle.
    let changed = changed || !errors.is_empty();

    ReloadOutcome {
        result: LoadResult {
            mocks,
            errors,
            // The walk scanned every `.json`/`.yaml`/`.yml` in the tree, but
            // a file claimed as a fixture is a body, not a mock definition —
            // the summary counts mocks.
            files: scanned.len() - fixture_files,
        },
        changed,
    }
}

/// Loads mock configurations from a directory or file.
///
/// Args:
///     path (str): Path to a directory containing `.json`/`.yaml`/`.yml` mock
///     files, or a single such file.
///
/// Returns:
///     MockStore: Thread-safe HashMap of mock configurations keyed by "METHOD:PATH".
///
/// Per-file load failures are logged here, one `warn!` each — the startup
/// path discards the [`LoadResult`] behind a [`MockStore`], so this is the
/// last chance anyone sees the messages. A caller that *does* keep the
/// result (hot reload, `mimic validate`) doesn't come through here, and owns
/// its own logging.
pub fn load_mocks(path: &str) -> MockStore {
    let result = load_mocks_map(path);
    for error in &result.errors {
        warn!(
            "Failed to load mock file {}: {}",
            error.target(),
            error.message
        );
    }
    Arc::new(RwLock::new(result.mocks))
}

/// Extensions the loader treats as a mock definition rather than an ordinary
/// file (or a fixture named by `response_file`). Which of these a given file
/// gets parsed with is decided in [`load_single_mock`], purely from this same
/// extension.
const MOCK_EXTENSIONS: [&str; 3] = ["json", "yaml", "yml"];

/// Whether `path`'s extension is one [`collect_mock_files`] treats as a mock
/// definition.
fn is_mock_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| MOCK_EXTENSIONS.contains(&ext))
}

/// How many directory levels the walk follows before giving up and reporting
/// a cycle rather than continuing silently.
///
/// This is a backstop for whatever `visited` can't catch — a dangling or
/// otherwise uncanonicalizable symlink — not a limit any real mocks tree
/// should come near. `visited`, keyed by canonical path, is what actually
/// stops an ordinary symlink cycle after one extra level.
const MAX_WALK_DEPTH: usize = 64;

/// Everything one level of [`collect_mock_files`]'s recursion needs, bundled
/// so adding a cross-cycle concern (the cache, the change flag) doesn't mean
/// adding another positional parameter to every recursive call.
struct ScanState<'a> {
    scanned: &'a mut Vec<(PathBuf, LoadedFile)>,
    errors: &'a mut Vec<LoadError>,
    visited: &'a mut HashSet<PathBuf>,
    cache: &'a mut LoaderCache,
    changed: &'a mut bool,
    seen: &'a mut HashSet<PathBuf>,
}

/// Recursively collects and loads all `.json`/`.yaml`/`.yml` mock files from a
/// directory tree.
///
/// Entries are **sorted by path before anything is loaded**. `fs::read_dir`
/// guarantees no ordering — it differs between ext4 and NTFS, and can change
/// on the same machine when unrelated files are added or removed — and two
/// things downstream read the resulting order as if it meant something: which
/// of several mocks sharing a `METHOD:path` wins a tie in `find_matching_mock`,
/// and (before mocks were given a stable identity) which mock owned which
/// counter. Sorting makes bucket order a pure function of the file names:
/// depth-first, alphabetical by full path, with a subdirectory visited at the
/// point its own name sorts in.
fn collect_mock_files(
    dir: &Path,
    root: &Path,
    max_response_file: u64,
    state: &mut ScanState,
    depth: usize,
) {
    if depth > MAX_WALK_DEPTH {
        let message = format!(
            "Mocks directory walk exceeded {} levels at {}; stopping here \
             (an unresolvable symlink cycle?)",
            MAX_WALK_DEPTH,
            dir.display()
        );
        error!("{}", message);
        state.errors.push(LoadError {
            file: dir.to_path_buf(),
            element: None,
            message,
        });
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            let message = format!("Failed to read directory {}: {}", dir.display(), e);
            error!("{}", message);
            state.errors.push(LoadError {
                file: dir.to_path_buf(),
                element: None,
                message,
            });
            return;
        }
    };

    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for entry_path in paths {
        if entry_path.is_dir() {
            // Canonicalizing resolves the symlink (if any) to the real
            // directory it names, so two different links to the same target
            // — or a link back to an ancestor — collide on the same key.
            // A path canonicalize can't resolve just isn't deduplicated; the
            // depth check above still bounds it.
            let canonical = entry_path
                .canonicalize()
                .unwrap_or_else(|_| entry_path.clone());
            if !state.visited.insert(canonical.clone()) {
                warn!(
                    "Symlink cycle detected: {} has already been walked (as {}); \
                     skipping to avoid loading its mocks again",
                    entry_path.display(),
                    canonical.display()
                );
                continue;
            }
            collect_mock_files(&entry_path, root, max_response_file, state, depth + 1);
        } else if entry_path.is_file() && is_mock_file(&entry_path) {
            state.seen.insert(entry_path.clone());
            let outcome = load_single_mock_maybe_cached(
                &entry_path,
                root,
                max_response_file,
                state.cache,
                state.changed,
            );
            state.scanned.push((entry_path, outcome));
        }
    }
}

/// [`load_single_mock`], skipped in favor of `cache` when both the mock file
/// and every fixture it depends on are exactly as they were last cycle.
///
/// A cache hit costs one `stat` for the mock file and one more per fixture it
/// references — no read of file contents. A miss reads and re-parses, then
/// updates `cache` (or evicts the entry, for a file that stopped loading —
/// `load_single_mock`'s failure path handles retrying it every cycle without
/// this function's help).
fn load_single_mock_maybe_cached(
    path: &Path,
    root: &Path,
    max_response_file: u64,
    cache: &mut LoaderCache,
    changed: &mut bool,
) -> LoadedFile {
    let fingerprint = fs::metadata(path)
        .ok()
        .and_then(|m| FileFingerprint::of(&m));

    if let Some(fp) = fingerprint {
        if let Some(cached) = cache.files.get(path) {
            if cached.fingerprint == fp && fixtures_unchanged(&cached.fixture_fingerprints) {
                return cached.outcome.clone();
            }
        }
    }

    *changed = true;
    let outcome = load_single_mock(path, root, max_response_file, &mut cache.fixtures);

    match (&outcome, fingerprint) {
        // Only a file whose every element loaded is cached: an element that
        // fails validation stays out, so the next cycle retries it rather
        // than freezing a "still broken" verdict under an unchanged
        // fingerprint.
        (outcome, Some(fp)) if outcome.fully_loaded() => {
            let fixture_fingerprints = outcome
                .fixtures()
                .into_iter()
                .filter_map(|f| {
                    fs::metadata(f)
                        .ok()
                        .and_then(|m| FileFingerprint::of(&m))
                        .map(|fp| (f.clone(), fp))
                })
                .collect();
            cache.files.insert(
                path.to_path_buf(),
                CachedMock {
                    fingerprint: fp,
                    fixture_fingerprints,
                    outcome: outcome.clone(),
                },
            );
        }
        _ => {
            // Parse failure, an element that failed validation, or a `stat`
            // that raced with a delete: don't cache it, so the next cycle
            // retries rather than serving a stale "still broken" verdict
            // forever.
            cache.files.remove(path);
        }
    }

    outcome
}

/// True when every fixture a cached mock depends on still matches the
/// fingerprint it had when that entry was cached — the check that catches a
/// `response_file` edited without its referencing mock file changing.
fn fixtures_unchanged(fingerprints: &[(PathBuf, FileFingerprint)]) -> bool {
    fingerprints.iter().all(|(path, fp)| {
        fs::metadata(path)
            .ok()
            .and_then(|m| FileFingerprint::of(&m))
            == Some(*fp)
    })
}

/// How a file — or one element of an array file — names itself in error
/// messages and warnings: `orders.json`, or `orders.json#2`.
fn source_label(path: &Path, element: Option<usize>) -> String {
    match element {
        Some(index) => format!("{}#{}", path.display(), index),
        None => path.display().to_string(),
    }
}

/// Register one loaded mock under its `METHOD:path` key, and warn when a
/// bucket gains a second member — the situation `find_matching_mock`'s
/// file-name tie-break was built for.
fn register_mock(
    mocks: &mut HashMap<String, Vec<MockConfig>>,
    file: &Path,
    element: Option<usize>,
    loaded: &LoadedMock,
) {
    let mock = &loaded.mock;
    let key = create_mock_key(&mock.method, &mock.path);
    debug!("Loaded mock: {} -> {}", key, source_label(file, element));
    let entry = mocks.entry(key).or_default();
    if !entry.is_empty() {
        warn!(
            "Multiple mocks registered for {} {}: {} total (next from {})",
            mock.method,
            mock.path,
            entry.len() + 1,
            source_label(file, element)
        );
    }
    entry.push(mock.clone());
}

/// What one mock file parsed into: a single mock (the shape everything has
/// always had) or a top-level array of them.
enum ParsedMockFile {
    One(Box<MockConfig>),
    Many(Vec<MockConfig>),
}

/// Parse `contents` into one or several [`MockConfig`]s, picking the parser
/// from `path`'s extension: `.yaml`/`.yml` goes through `serde_yaml`,
/// everything else (including plain `.json`) through `serde_json`. Both
/// parsers deserialize into the same `MockConfig`, so a mock means exactly
/// the same thing in either format — and a file holding several of them
/// means the same thing too.
fn parse_mock_config(path: &Path, contents: &str) -> Result<ParsedMockFile, String> {
    let single_parse_error = |e| {
        format!(
            "Failed to parse {} in {}: {}",
            ext_word(path),
            path.display(),
            e
        )
    };

    match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml") | Some("yml") => {
            // YAML's top-level shape can't be told apart lexically — a file
            // may start with comments, `---`, or spell its sequence in block
            // form (`- method: ...`) — so parse once into a `Value` and let
            // the parsed data answer: a sequence is an array of mocks,
            // anything else one mock.
            let value: serde_yaml::Value = serde_yaml::from_str(contents)
                .map_err(|e| format!("Failed to parse YAML in {}: {}", path.display(), e))?;
            match value {
                serde_yaml::Value::Sequence(items) => {
                    serde_yaml::from_value(serde_yaml::Value::Sequence(items))
                        .map(ParsedMockFile::Many)
                        .map_err(|e| format!("Failed to parse YAML in {}: {}", path.display(), e))
                }
                other => serde_yaml::from_value(other)
                    .map(ParsedMockFile::One)
                    .map_err(|e| format!("Failed to parse YAML in {}: {}", path.display(), e)),
            }
        }
        _ => {
            // JSON has no comments or prelude, so the first non-whitespace
            // byte is the whole story: `[` is an array of mocks, anything
            // else a single mock, with its parse errors unchanged.
            if contents.trim_start().starts_with('[') {
                serde_json::from_str(contents)
                    .map(ParsedMockFile::Many)
                    .map_err(single_parse_error)
            } else {
                serde_json::from_str(contents)
                    .map(|mock| ParsedMockFile::One(Box::new(mock)))
                    .map_err(single_parse_error)
            }
        }
    }
}

/// The format word a file's parse error names: `JSON` or `YAML`.
fn ext_word(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml") | Some("yml") => "YAML",
        _ => "JSON",
    }
}

/// Loads a single mock configuration from a `.json`, `.yaml`, or `.yml` file.
///
/// `root` is the mocks root a `response_file` may not escape; `max_response_file`
/// caps how large one of those files may be.
///
/// Returns every mock the file holds along with the fixture files each claims.
/// A file that isn't readable or parseable fails as a whole; a file that is
/// an array of mocks fails per element, so one bad sibling doesn't stop the
/// rest from loading.
fn load_single_mock(
    path: &Path,
    root: &Path,
    max_response_file: u64,
    fixture_cache: &mut HashMap<PathBuf, (FileFingerprint, Bytes)>,
) -> LoadedFile {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            return LoadedFile::Failed(format!("Failed to read file {}: {}", path.display(), e))
        }
    };

    let parsed = match parse_mock_config(path, &contents) {
        Ok(parsed) => parsed,
        Err(message) => return LoadedFile::Failed(message),
    };

    // A single-object file keeps index `None`: its identity, `source`, and
    // sequence state are exactly what they always were. Array elements are
    // identified positionally — the best identity a mock in an array has.
    match parsed {
        ParsedMockFile::One(mock) => LoadedFile::Parsed {
            array: false,
            outcomes: vec![load_mock_element(
                *mock,
                path,
                None,
                root,
                max_response_file,
                fixture_cache,
            )],
        },
        ParsedMockFile::Many(mocks) => LoadedFile::Parsed {
            array: true,
            outcomes: mocks
                .into_iter()
                .enumerate()
                .map(|(index, mock)| {
                    load_mock_element(
                        mock,
                        path,
                        Some(index),
                        root,
                        max_response_file,
                        fixture_cache,
                    )
                })
                .collect(),
        },
    }
}

/// Load one [`MockConfig`]: validate it, resolve and read any fixture it
/// claims, and stamp loader-owned identity onto it.
///
/// `element` is `None` for a single-object file and `Some(i)` for element i
/// of an array file; the labels `source` and error messages carry differ
/// only by that `#i` suffix (`orders.json` vs `orders.json#2`). Fixtures
/// resolve against `path`'s directory exactly as they always have — an
/// element lives in the same file, so it resolves to the same places.
fn load_mock_element(
    mut mock: MockConfig,
    path: &Path,
    element: Option<usize>,
    root: &Path,
    max_response_file: u64,
    fixture_cache: &mut HashMap<PathBuf, (FileFingerprint, Bytes)>,
) -> Result<LoadedMock, String> {
    let label = source_label(path, element);

    // The file this mock came from is the answer to "where do I go to change
    // this?", so it's recorded here rather than dropped after the parse.
    // Assigned unconditionally: `source` is loader-owned, and a value written
    // into the mock JSON by hand must not be able to misattribute a mock.
    // The element index travels alongside it — also loader-owned, since it
    // is decided by the file's shape, not by anything it says.
    mock.source = Some(path.display().to_string());
    mock.source_index = element;

    // Validate required fields
    if mock.method.is_empty() {
        return Err(format!("Empty method in {}", label));
    }
    if mock.path.is_empty() {
        return Err(format!("Empty path in {}", label));
    }

    // Response bodies read from disk. Also loader-owned: whatever a mock file
    // says about `response_bytes` is dropped by serde before we get here.
    mock.response_bytes = None;
    let mut fixtures = Vec::new();

    if let Some(file) = mock.response_file.clone() {
        reject_both_bodies(&mock.response, &file, &label)?;
        let (resolved, bytes) =
            read_response_file(&file, path, &label, root, max_response_file, fixture_cache)?;
        mock.response_bytes = Some(bytes);
        fixtures.push(resolved);
    }

    if !crate::types::is_valid_status(mock.status) {
        warn!(
            "{}: status {} is outside 100-599 and will be served as 200 OK",
            label, mock.status
        );
    }

    for (index, step) in mock.sequence.iter_mut().flatten().enumerate() {
        step.response_bytes = None;
        if !crate::types::is_valid_status(step.status) {
            warn!(
                "{}: sequence step {} has status {}, outside 100-599, and will be served as 200 OK",
                label, index, step.status
            );
        }
        let Some(file) = step.response_file.clone() else {
            continue;
        };
        reject_both_bodies(&step.response, &file, &label)
            .map_err(|e| format!("{} (sequence step {})", e, index))?;
        let (resolved, bytes) =
            read_response_file(&file, path, &label, root, max_response_file, fixture_cache)
                .map_err(|e| format!("{} (sequence step {})", e, index))?;
        step.response_bytes = Some(bytes);
        fixtures.push(resolved);
    }

    Ok(LoadedMock { mock, fixtures })
}

/// `response` and `response_file` are two answers to one question, so a mock
/// that gives both is rejected rather than having one silently win.
fn reject_both_bodies(response: &serde_json::Value, file: &str, label: &str) -> Result<(), String> {
    if response.is_null() {
        return Ok(());
    }
    Err(format!(
        "{} sets both `response` and `response_file` ({}); use one or the other",
        label, file
    ))
}

/// Resolve `file` against the directory `mock_path` lives in, refuse anything
/// outside `root`, enforce `max_response_file`, and read the bytes.
///
/// Resolution is relative to the mock file rather than to the process working
/// directory, so a mocks tree stays relocatable and a Docker volume mount works
/// unchanged. Containment is checked *after* canonicalization, so `..` segments
/// and symlinks are both covered — this is a server pointed at a directory it
/// doesn't own, and "the fixture path is user input" is the whole point.
///
/// `label` is how the owning mock names itself in errors — `orders.json`, or
/// `orders.json#2` when it is one element of an array file.
fn read_response_file(
    file: &str,
    mock_path: &Path,
    label: &str,
    root: &Path,
    max_response_file: u64,
    fixture_cache: &mut HashMap<PathBuf, (FileFingerprint, Bytes)>,
) -> Result<(PathBuf, Bytes), String> {
    let base = mock_path.parent().unwrap_or(Path::new("."));
    let candidate = base.join(file);

    let resolved = candidate.canonicalize().map_err(|e| {
        format!(
            "{}: cannot read response_file '{}' ({}): {}",
            label,
            file,
            candidate.display(),
            e
        )
    })?;

    if !resolved.starts_with(root) {
        return Err(format!(
            "{}: response_file '{}' resolves to {}, which is outside the mocks root {}",
            label,
            file,
            resolved.display(),
            root.display()
        ));
    }

    let metadata = fs::metadata(&resolved)
        .map_err(|e| format!("{}: cannot stat response_file '{}': {}", label, file, e))?;
    if !metadata.is_file() {
        return Err(format!(
            "{}: response_file '{}' is not a regular file",
            label, file
        ));
    }
    // Checked against the file's size before reading, so an oversized fixture
    // costs a stat rather than a 2 GB allocation.
    if max_response_file > 0 && metadata.len() > max_response_file {
        return Err(format!(
            "{}: response_file '{}' is {} bytes, over the {}-byte {} limit; skipping this mock",
            label,
            file,
            metadata.len(),
            max_response_file,
            MAX_RESPONSE_FILE_ENV
        ));
    }

    // A fixture this same walk has already read, unchanged since then, is
    // served from `fixture_cache` rather than read again — this is what
    // keeps an edit to the *mock* file from forcing a re-read of a fixture it
    // references but didn't touch, and what lets two mocks sharing one
    // fixture pay for the read once.
    if let Some(fingerprint) = FileFingerprint::of(&metadata) {
        if let Some((cached_fingerprint, cached_bytes)) = fixture_cache.get(&resolved) {
            if *cached_fingerprint == fingerprint {
                return Ok((resolved, cached_bytes.clone()));
            }
        }

        let bytes = fs::read(&resolved)
            .map_err(|e| format!("{}: failed to read response_file '{}': {}", label, file, e))?;
        let bytes = Bytes::from(bytes);

        debug!(
            "Loaded response_file {} ({} bytes) for {}",
            resolved.display(),
            bytes.len(),
            label
        );

        fixture_cache.insert(resolved.clone(), (fingerprint, bytes.clone()));
        return Ok((resolved, bytes));
    }

    // Metadata lacks a usable modified time (some platforms/filesystems):
    // caching would be unsound, so just read it every time, as before.
    let bytes = fs::read(&resolved)
        .map_err(|e| format!("{}: failed to read response_file '{}': {}", label, file, e))?;

    debug!(
        "Loaded response_file {} ({} bytes) for {}",
        resolved.display(),
        bytes.len(),
        label
    );

    Ok((resolved, Bytes::from(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MockIdentity, MockOrigin};

    /// #137: `cache.fixtures` is keyed by fixture path and was never pruned, so
    /// a deleted fixture's bytes stayed in memory for the life of the process.
    #[test]
    fn hot_reload_evicts_cached_fixtures_nobody_claims_any_more() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("body.txt"), "hello").unwrap();
        std::fs::write(
            dir.path().join("m.json"),
            r#"{"method":"GET","path":"/f","status":200,"response_file":"body.txt"}"#,
        )
        .unwrap();
        let path = dir.path().to_str().unwrap();
        let mut cache = LoaderCache::new();

        let first = load_mocks_map_hot_reload(path, &mut cache);
        assert!(first.result.errors.is_empty(), "{:?}", first.result.errors);
        assert_eq!(cache.fixtures.len(), 1);

        std::fs::remove_file(dir.path().join("m.json")).unwrap();
        std::fs::remove_file(dir.path().join("body.txt")).unwrap();
        let second = load_mocks_map_hot_reload(path, &mut cache);
        assert!(second.changed);
        assert!(
            cache.fixtures.is_empty(),
            "a fixture no mock claims must not stay cached"
        );
    }

    use serde_json::json;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_load_mocks_from_directory() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create mock files
        let mock1 = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;

        let mock2 = r#"{
            "method": "POST",
            "path": "/login",
            "status": 201,
            "response": {"token": "abc123"}
        }"#;

        let file1_path = dir_path.join("mock1.json");
        let file2_path = dir_path.join("mock2.json");

        let mut file1 = File::create(&file1_path).unwrap();
        file1.write_all(mock1.as_bytes()).unwrap();

        let mut file2 = File::create(&file2_path).unwrap();
        file2.write_all(mock2.as_bytes()).unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.mocks.len(), 2);
        assert!(result.mocks.contains_key("GET:/users"));
        assert!(result.mocks.contains_key("POST:/login"));
    }

    #[test]
    fn test_load_mocks_reads_scenario_tags() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        let tagged = r#"{
            "method": "POST",
            "path": "/checkout",
            "status": 500,
            "tags": ["error-scenario"],
            "response": {"error": "internal error"}
        }"#;
        let untagged = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;

        File::create(dir_path.join("checkout_500.json"))
            .unwrap()
            .write_all(tagged.as_bytes())
            .unwrap();
        File::create(dir_path.join("users.json"))
            .unwrap()
            .write_all(untagged.as_bytes())
            .unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        assert_eq!(
            result.mocks["POST:/checkout"][0].tags,
            vec!["error-scenario"]
        );
        // A file written before tags existed still loads, with no tags.
        assert!(result.mocks["GET:/users"][0].tags.is_empty());
    }

    /// `mocks/sub/loop -> ..` points straight back at the mocks root. Without
    /// the visited-set guard, the walk would descend into it, find `loop`
    /// again, descend again, and register `users.json` once per level.
    #[cfg(unix)]
    #[test]
    fn test_symlink_cycle_loads_each_mock_exactly_once() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        fs::create_dir(dir_path.join("sub")).unwrap();
        File::create(dir_path.join("users.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/users","status":200,"response":[{"id":1}]}"#)
            .unwrap();
        std::os::unix::fs::symlink("..", dir_path.join("sub/loop")).unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());

        assert_eq!(
            result.mocks.get("GET:/users").map(|v| v.len()),
            Some(1),
            "the mock behind the cycle must be registered exactly once"
        );
    }

    /// A symlink to a directory *outside* the mocks tree, with no cycle, is
    /// exactly the sharing-fixtures-between-projects use case — it must keep
    /// working.
    #[cfg(unix)]
    #[test]
    fn test_symlink_to_an_external_directory_with_no_cycle_still_loads() {
        let shared = TempDir::new().unwrap();
        File::create(shared.path().join("shared.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/shared","status":200,"response":{"ok":true}}"#)
            .unwrap();

        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();
        File::create(dir_path.join("local.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/local","status":200,"response":{}}"#)
            .unwrap();
        std::os::unix::fs::symlink(shared.path(), dir_path.join("linked")).unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());

        assert_eq!(result.errors.len(), 0);
        assert!(result.mocks.contains_key("GET:/local"));
        assert!(result.mocks.contains_key("GET:/shared"));
    }

    /// Two different symlinks pointing at the same real directory must not
    /// double-load its mocks either — the guard is by canonical path, not by
    /// the specific link that led there.
    #[cfg(unix)]
    #[test]
    fn test_two_symlinks_to_the_same_directory_load_its_mocks_once() {
        let shared = TempDir::new().unwrap();
        File::create(shared.path().join("shared.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/shared","status":200,"response":{"ok":true}}"#)
            .unwrap();

        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();
        std::os::unix::fs::symlink(shared.path(), dir_path.join("a")).unwrap();
        std::os::unix::fs::symlink(shared.path(), dir_path.join("b")).unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());

        assert_eq!(result.mocks.get("GET:/shared").map(|v| v.len()), Some(1));
    }

    // ── Hot-reload change detection (#110) ──────────────────────────────

    #[test]
    fn test_hot_reload_reports_unchanged_on_an_idle_second_cycle() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("users.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/users","status":200,"response":[]}"#)
            .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(first.changed, "the first cycle always reports a change");
        assert!(first.result.mocks.contains_key("GET:/users"));

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(
            !second.changed,
            "nothing touched any file between the two cycles"
        );
        assert!(second.result.mocks.contains_key("GET:/users"));
    }

    #[test]
    fn test_hot_reload_detects_an_edited_mock_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("users.json");
        File::create(&file)
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/users","status":200,"response":[]}"#)
            .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert_eq!(first.result.mocks["GET:/users"][0].status, 200);

        // Same-status edit, different byte length: the fingerprint has
        // something to notice even on a filesystem whose mtimes are too
        // coarse for two writes inside the same tick, which a same-length
        // edit (`200` → `201`) would otherwise race.
        File::create(&file)
            .unwrap()
            .write_all(
                br#"{"method":"GET","path":"/users","status":201,"response":[],"note":"edited"}"#,
            )
            .unwrap();

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(second.changed, "the edited file must be detected");
        assert_eq!(second.result.mocks["GET:/users"][0].status, 201);
    }

    #[test]
    fn test_hot_reload_detects_an_added_file() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("users.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/users","status":200,"response":[]}"#)
            .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert_eq!(first.result.mocks.len(), 1);

        File::create(dir.path().join("orders.json"))
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/orders","status":200,"response":[]}"#)
            .unwrap();

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(second.changed, "a new file must be detected");
        assert!(second.result.mocks.contains_key("GET:/orders"));
    }

    #[test]
    fn test_hot_reload_detects_a_deleted_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("users.json");
        File::create(&file)
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/users","status":200,"response":[]}"#)
            .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(first.result.mocks.contains_key("GET:/users"));

        fs::remove_file(&file).unwrap();

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(second.changed, "a deleted file must be detected");
        assert!(!second.result.mocks.contains_key("GET:/users"));
    }

    /// The case #110 calls out by name: editing a `response_file` fixture
    /// must be picked up even though the mock file that references it never
    /// changed.
    #[test]
    fn test_hot_reload_detects_an_edited_response_file_even_though_the_mock_file_did_not_change() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("download.json"))
            .unwrap()
            .write_all(
                br#"{"method":"GET","path":"/download","status":200,"response_file":"body.bin"}"#,
            )
            .unwrap();
        File::create(dir.path().join("body.bin"))
            .unwrap()
            .write_all(b"original")
            .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert_eq!(
            first.result.mocks["GET:/download"][0]
                .response_bytes
                .as_deref(),
            Some(b"original".as_slice())
        );

        // Only the fixture changes — the mock.json that names it is untouched.
        File::create(dir.path().join("body.bin"))
            .unwrap()
            .write_all(b"a different, longer body")
            .unwrap();

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(
            second.changed,
            "a fixture edit must be detected even though its mock file didn't change"
        );
        assert_eq!(
            second.result.mocks["GET:/download"][0]
                .response_bytes
                .as_deref(),
            Some(b"a different, longer body".as_slice())
        );
    }

    #[test]
    fn test_load_mocks_nonexistent_directory() {
        let result = load_mocks_map("/nonexistent/path");
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_load_mocks_empty_path() {
        let temp_dir = TempDir::new().unwrap();
        let result = load_mocks_map(temp_dir.path().to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors.len(), 0);
    }

    #[test]
    fn test_load_mocks_ignores_non_json_files() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create a non-JSON file
        let txt_file = dir_path.join("readme.txt");
        let mut file = File::create(&txt_file).unwrap();
        file.write_all(b"This is not JSON").unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors.len(), 0);
    }

    #[test]
    fn test_load_mocks_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("invalid.json");

        let mut file = File::create(&file_path).unwrap();
        file.write_all(b"{invalid json}").unwrap();

        let result = load_mocks_map(temp_dir.path().to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_load_mocks_empty_method() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("empty_method.json");

        let mock = r#"{
            "method": "",
            "path": "/test",
            "status": 200,
            "response": {}
        }"#;

        let mut file = File::create(&file_path).unwrap();
        file.write_all(mock.as_bytes()).unwrap();

        let result = load_mocks_map(temp_dir.path().to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_load_mocks_from_subdirectory() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create a mock in the top-level directory
        let mock1 = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;
        let mut file1 = File::create(dir_path.join("top.json")).unwrap();
        file1.write_all(mock1.as_bytes()).unwrap();

        // Create a subdirectory with a mock
        let sub_dir = dir_path.join("advanced");
        fs::create_dir(&sub_dir).unwrap();
        let mock2 = r#"{
            "method": "POST",
            "path": "/login",
            "status": 200,
            "response": {"token": "abc"}
        }"#;
        let mut file2 = File::create(sub_dir.join("login.json")).unwrap();
        file2.write_all(mock2.as_bytes()).unwrap();

        // Create a nested subdirectory
        let nested_dir = sub_dir.join("nested");
        fs::create_dir(&nested_dir).unwrap();
        let mock3 = r#"{
            "method": "DELETE",
            "path": "/items/1",
            "status": 204,
            "response": {}
        }"#;
        let mut file3 = File::create(nested_dir.join("delete.json")).unwrap();
        file3.write_all(mock3.as_bytes()).unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.mocks.len(), 3);
        assert!(result.mocks.contains_key("GET:/users"));
        assert!(result.mocks.contains_key("POST:/login"));
        assert!(result.mocks.contains_key("DELETE:/items/1"));
    }

    #[test]
    fn test_load_mocks_file_not_directory() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("not_a_dir.txt");
        File::create(&file_path).unwrap();

        let result = load_mocks_map(file_path.to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_load_single_mock_valid() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.json");

        let mock = r#"{
            "method": "POST",
            "path": "/api/test",
            "status": 201,
            "response": {"success": true}
        }"#;

        let mut file = File::create(&file_path).unwrap();
        file.write_all(mock.as_bytes()).unwrap();

        let result = load_single_mock(
            &file_path,
            temp_dir.path(),
            DEFAULT_MAX_RESPONSE_FILE,
            &mut HashMap::new(),
        );
        let LoadedFile::Parsed { array, outcomes } = result else {
            panic!("a single-object file parses into its single outcome");
        };
        assert!(!array, "the top level was not an array");
        assert_eq!(outcomes.len(), 1);

        let mock_config = &outcomes[0].as_ref().expect("the one mock loads").mock;
        assert_eq!(mock_config.method, "POST");
        assert_eq!(mock_config.path, "/api/test");
        assert_eq!(mock_config.status, 201);
        assert_eq!(
            mock_config.source.as_deref(),
            Some(file_path.to_str().unwrap())
        );
        assert_eq!(mock_config.source_index, None);
    }

    #[test]
    fn test_load_mocks_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        for i in 1..=5 {
            let mock = format!(
                r#"{{
                    "method": "GET",
                    "path": "/endpoint{}",
                    "status": 200,
                    "response": {{"id": {}}}
                }}"#,
                i, i
            );

            let file_path = dir_path.join(format!("mock{}.json", i));
            let mut file = File::create(&file_path).unwrap();
            file.write_all(mock.as_bytes()).unwrap();
        }

        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.mocks.len(), 5);
    }

    #[test]
    fn test_create_mock_key_from_config() {
        let mock = MockConfig {
            method: "PUT".to_string(),
            path: "/api/users/1".to_string(),
            status: 200,
            response: json!({}),
            consume_body: true,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        };

        let key = create_mock_key(&mock.method, &mock.path);
        assert_eq!(key, "PUT:/api/users/1");
    }

    #[test]
    fn test_load_mocks_multiple_files_same_path_retained() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Two mocks for the same METHOD:PATH (intended for body matching)
        let mock_admin = r#"{
            "method": "POST",
            "path": "/login",
            "status": 200,
            "response": {"role": "admin"},
            "body": {"type": "json", "partial": {"role": "admin"}}
        }"#;

        let mock_user = r#"{
            "method": "POST",
            "path": "/login",
            "status": 200,
            "response": {"role": "user"},
            "body": {"type": "json", "partial": {"role": "user"}}
        }"#;

        let mut file1 = File::create(dir_path.join("login_admin.json")).unwrap();
        file1.write_all(mock_admin.as_bytes()).unwrap();

        let mut file2 = File::create(dir_path.join("login_user.json")).unwrap();
        file2.write_all(mock_user.as_bytes()).unwrap();

        let result = load_mocks_map(dir_path.to_str().unwrap());
        // One unique key, but two mocks stored under it
        assert_eq!(result.mocks.len(), 1);
        assert_eq!(result.mocks["POST:/login"].len(), 2);
    }

    #[tokio::test]
    async fn test_reload_mocks_reflects_file_changes() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create initial mock file
        let mock1 = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;
        let mut file1 = File::create(dir_path.join("users.json")).unwrap();
        file1.write_all(mock1.as_bytes()).unwrap();

        // Load initial state
        let store = load_mocks(dir_path.to_str().unwrap());
        {
            let mocks = store.read().await;
            assert_eq!(mocks.len(), 1);
            assert!(mocks.contains_key("GET:/users"));
        }

        // Add a new mock file (simulating a file change)
        let mock2 = r#"{
            "method": "POST",
            "path": "/login",
            "status": 201,
            "response": {"token": "abc123"}
        }"#;
        let mut file2 = File::create(dir_path.join("login.json")).unwrap();
        file2.write_all(mock2.as_bytes()).unwrap();

        // Reload mocks into the store (simulating hot reload)
        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        {
            let mut mocks = store.write().await;
            *mocks = result.mocks;
        }

        // Verify the new mock is now available
        {
            let mocks = store.read().await;
            assert_eq!(mocks.len(), 2);
            assert!(mocks.contains_key("GET:/users"));
            assert!(mocks.contains_key("POST:/login"));
        }

        // Delete the first mock file (simulating a file removal)
        fs::remove_file(dir_path.join("users.json")).unwrap();

        // Reload mocks again
        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        {
            let mut mocks = store.write().await;
            *mocks = result.mocks;
        }

        // Verify only the remaining mock is available
        {
            let mocks = store.read().await;
            assert_eq!(mocks.len(), 1);
            assert!(!mocks.contains_key("GET:/users"));
            assert!(mocks.contains_key("POST:/login"));
        }
    }

    #[tokio::test]
    async fn test_reload_skips_on_errors() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create a valid mock file
        let mock1 = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;
        let mut file1 = File::create(dir_path.join("users.json")).unwrap();
        file1.write_all(mock1.as_bytes()).unwrap();

        // Load initial state
        let store = load_mocks(dir_path.to_str().unwrap());
        {
            let mocks = store.read().await;
            assert_eq!(mocks.len(), 1);
        }

        // Add an invalid mock file
        let mut file2 = File::create(dir_path.join("broken.json")).unwrap();
        file2.write_all(b"{invalid json}").unwrap();

        // Reload should report errors; caller should skip the swap
        let result = load_mocks_map(dir_path.to_str().unwrap());
        assert!(!result.errors.is_empty());
        // Do NOT swap — previous mock set is preserved
        {
            let mocks = store.read().await;
            assert_eq!(mocks.len(), 1);
            assert!(mocks.contains_key("GET:/users"));
        }
    }

    #[test]
    fn test_loader_records_the_source_file() {
        // `/admin/mocks` answers "where do I go to change this?", which needs
        // the file the mock came from rather than just its contents.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("get_users.json");
        fs::write(
            &file,
            r#"{"method":"GET","path":"/users","status":200,"response":{"ok":true}}"#,
        )
        .unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        let mock = &result.mocks["GET:/users"][0];
        assert_eq!(mock.source.as_deref(), Some(file.to_str().unwrap()));
    }

    #[test]
    fn test_loader_records_the_source_of_a_single_file_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one.json");
        fs::write(
            &file,
            r#"{"method":"POST","path":"/login","status":201,"response":{}}"#,
        )
        .unwrap();

        let result = load_mocks_map(file.to_str().unwrap());
        assert_eq!(
            result.mocks["POST:/login"][0].source.as_deref(),
            Some(file.to_str().unwrap())
        );
    }

    // ------------------------------------------------------------------
    // Deterministic load order (#86)
    // ------------------------------------------------------------------

    /// Write files in the given order and return the bucket for `GET:/dup`,
    /// as the markers its mocks carry.
    fn load_dup_bucket_written_in(order: &[&str]) -> Vec<String> {
        let dir = TempDir::new().unwrap();
        for name in order {
            fs::write(
                dir.path().join(format!("{}.json", name)),
                format!(
                    r#"{{"method":"GET","path":"/dup","status":200,"response":{{"from":"{}"}}}}"#,
                    name
                ),
            )
            .unwrap();
        }

        let result = load_mocks_map(dir.path().to_str().unwrap());
        result.mocks["GET:/dup"]
            .iter()
            .map(|mock| mock.response["from"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn test_bucket_order_does_not_depend_on_the_order_files_were_created() {
        // `fs::read_dir` guarantees no ordering, and `find_matching_mock`'s
        // final tie-break is bucket position — so without sorting, which of
        // two identical mocks wins is decided by the filesystem, and can flip
        // on a fresh clone or a rebuilt image with no diff to point at.
        let forwards = load_dup_bucket_written_in(&["a", "m", "z"]);
        let backwards = load_dup_bucket_written_in(&["z", "m", "a"]);
        let shuffled = load_dup_bucket_written_in(&["m", "z", "a"]);

        assert_eq!(forwards, vec!["a", "m", "z"]);
        assert_eq!(forwards, backwards);
        assert_eq!(forwards, shuffled);
    }

    #[test]
    fn test_nested_directories_load_in_a_fixed_alphabetical_order() {
        // The rule covers the interleaving of a directory's own files with its
        // subdirectories', not just the files at one level.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("advanced");
        fs::create_dir(&sub).unwrap();

        for (path, marker) in [
            (dir.path().join("b_top.json"), "b_top"),
            (dir.path().join("z_top.json"), "z_top"),
            (sub.join("nested.json"), "advanced/nested"),
        ] {
            fs::write(
                &path,
                format!(
                    r#"{{"method":"GET","path":"/dup","status":200,"response":{{"from":"{}"}}}}"#,
                    marker
                ),
            )
            .unwrap();
        }

        let result = load_mocks_map(dir.path().to_str().unwrap());
        let order: Vec<&str> = result.mocks["GET:/dup"]
            .iter()
            .map(|mock| mock.response["from"].as_str().unwrap())
            .collect();

        // "advanced" sorts before "b_top.json" and "z_top.json", and is
        // recursed into at the point its own name sorts in.
        assert_eq!(order, vec!["advanced/nested", "b_top", "z_top"]);
    }

    // ------------------------------------------------------------------
    // YAML mock files (#116)
    // ------------------------------------------------------------------

    #[test]
    fn test_a_yaml_mock_loads_and_serves_identically_to_its_json_equivalent() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("users.yaml"),
            "method: GET\npath: /users\nstatus: 200\nresponse:\n  users: []\n",
        )
        .unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        let mock = &result.mocks["GET:/users"][0];
        assert_eq!(mock.method, "GET");
        assert_eq!(mock.status, 200);
        assert_eq!(mock.response, json!({"users": []}));
    }

    #[test]
    fn test_yml_extension_is_also_recognized() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("ping.yml"),
            "method: GET\npath: /ping\nstatus: 200\nresponse: {}\n",
        )
        .unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        assert!(result.mocks.contains_key("GET:/ping"));
    }

    #[test]
    fn test_json_and_yaml_mocks_coexist_with_deterministic_ordering() {
        // Same rule as #86 (deterministic load order), now exercised across
        // formats: bucket order is a function of the full path, regardless of
        // which extension a given file happens to use.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.yaml"),
            "method: GET\npath: /dup\nstatus: 200\nresponse:\n  from: a\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("m.json"),
            r#"{"method":"GET","path":"/dup","status":200,"response":{"from":"m"}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("z.yml"),
            "method: GET\npath: /dup\nstatus: 200\nresponse:\n  from: z\n",
        )
        .unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        let order: Vec<&str> = result.mocks["GET:/dup"]
            .iter()
            .map(|mock| mock.response["from"].as_str().unwrap())
            .collect();
        assert_eq!(order, vec!["a", "m", "z"]);
    }

    #[test]
    fn test_an_invalid_yaml_file_is_reported_without_blocking_other_mocks() {
        // Mirrors `test_reload_skips_on_errors`'s JSON case: one broken file
        // is a per-file error, not a reason to drop everything else that
        // parsed fine.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("users.json"),
            r#"{"method":"GET","path":"/users","status":200,"response":{"users":[]}}"#,
        )
        .unwrap();
        // Unbalanced flow mapping — invalid YAML.
        fs::write(dir.path().join("broken.yaml"), "method: [GET\n").unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors.len(), 1);
        assert!(result.mocks.contains_key("GET:/users"));
    }

    #[test]
    fn test_yaml_comments_and_block_scalars_are_supported() {
        // The two things JSON can't do that motivate YAML mocks at all:
        // inline documentation, and a literal block for a multi-line body.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("page.yaml"),
            "# Why this mock exists: the docs page ships static HTML.\n\
             method: GET\n\
             path: /page\n\
             status: 200\n\
             response:\n\
             \x20 html: |\n\
             \x20   <html>\n\
             \x20     <body>hi</body>\n\
             \x20   </html>\n",
        )
        .unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        let mock = &result.mocks["GET:/page"][0];
        assert_eq!(
            mock.response["html"],
            "<html>\n  <body>hi</body>\n</html>\n"
        );
    }

    #[test]
    fn test_response_file_resolution_is_unchanged_for_a_yaml_mock() {
        // `response_file` is read by the loader itself, independent of which
        // format the referencing mock file is written in — so the same
        // containment and byte-for-byte read behavior applies.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("body.txt"), "hello from disk").unwrap();
        fs::write(
            dir.path().join("download.yaml"),
            "method: GET\npath: /download\nstatus: 200\nresponse_file: body.txt\n",
        )
        .unwrap();

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors.len(), 0);
        let mock = &result.mocks["GET:/download"][0];
        assert_eq!(
            mock.response_bytes.as_deref(),
            Some(b"hello from disk".as_slice())
        );
    }

    // ------------------------------------------------------------------
    // Mocks directory resolution (#84)
    // ------------------------------------------------------------------

    /// A filesystem stub: only the listed paths exist.
    fn only<'a>(existing: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |path: &str| existing.contains(&path)
    }

    #[test]
    fn test_docker_default_is_unchanged_when_the_mount_point_exists() {
        // The whole point of probing /app/mocks first: every published
        // `docker run -v ./mocks:/app/mocks` command has to keep working
        // byte-for-byte.
        let resolved = resolve_mocks_dir_from(None, only(&[DOCKER_MOCKS_DIR, LOCAL_MOCKS_DIR]));
        assert_eq!(resolved.path, DOCKER_MOCKS_DIR);
        assert_eq!(resolved.origin, MocksDirOrigin::Docker);
        assert!(resolved.exists);
    }

    #[test]
    fn test_falls_back_to_local_mocks_outside_docker() {
        // `cargo run` from a fresh clone: /app/mocks is not a directory
        // anyone has, ./mocks is the one the repo ships.
        let resolved = resolve_mocks_dir_from(None, only(&[LOCAL_MOCKS_DIR]));
        assert_eq!(resolved.path, LOCAL_MOCKS_DIR);
        assert_eq!(resolved.origin, MocksDirOrigin::Local);
        assert!(resolved.exists);
    }

    #[test]
    fn test_falls_back_to_local_mocks_even_when_nothing_exists() {
        // Nothing to read, but the path reported has to be the one a user can
        // act on — `./mocks`, not a Docker path they'll never have.
        let resolved = resolve_mocks_dir_from(None, only(&[]));
        assert_eq!(resolved.path, LOCAL_MOCKS_DIR);
        assert!(!resolved.exists);
    }

    #[test]
    fn test_env_var_wins_over_both_defaults() {
        let resolved = resolve_mocks_dir_from(
            Some("/srv/fixtures".to_string()),
            only(&["/srv/fixtures", DOCKER_MOCKS_DIR, LOCAL_MOCKS_DIR]),
        );
        assert_eq!(resolved.path, "/srv/fixtures");
        assert_eq!(resolved.origin, MocksDirOrigin::Configured);
    }

    #[test]
    fn test_env_var_is_honored_even_when_it_does_not_exist() {
        // Silently falling back would leave a typo'd MIMIC_MOCKS_DIR looking
        // exactly like a working one that happens to be empty.
        let resolved = resolve_mocks_dir_from(
            Some("/typo/mocks".to_string()),
            only(&[DOCKER_MOCKS_DIR, LOCAL_MOCKS_DIR]),
        );
        assert_eq!(resolved.path, "/typo/mocks");
        assert_eq!(resolved.origin, MocksDirOrigin::Configured);
        assert!(!resolved.exists);
    }

    #[test]
    fn test_env_var_is_trimmed_and_an_empty_value_means_unset() {
        let trimmed = resolve_mocks_dir_from(Some("  /srv/fixtures  ".to_string()), only(&[]));
        assert_eq!(trimmed.path, "/srv/fixtures");

        // `MIMIC_MOCKS_DIR=` in a .env file must not resolve to "".
        for blank in ["", "   "] {
            let resolved =
                resolve_mocks_dir_from(Some(blank.to_string()), only(&[LOCAL_MOCKS_DIR]));
            assert_eq!(
                resolved.path, LOCAL_MOCKS_DIR,
                "an empty {} should behave as if it were unset",
                MOCKS_DIR_ENV
            );
        }
    }

    #[test]
    fn test_resolved_directory_actually_loads_the_mocks_in_it() {
        // The end-to-end shape of #84: resolve, then load, and get mocks.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("get_users.json"),
            r#"{"method":"GET","path":"/users","status":200,"response":{"users":[]}}"#,
        )
        .unwrap();

        let resolved = resolve_mocks_dir_from(Some(dir.path().display().to_string()), |p| {
            Path::new(p).exists()
        });
        assert!(resolved.exists);

        let result = load_mocks_map(&resolved.path);
        assert_eq!(result.errors.len(), 0);
        assert!(result.mocks.contains_key("GET:/users"));
    }

    #[test]
    fn test_loader_overrides_a_source_written_into_the_mock_file() {
        // `source` is loader-owned. A value typed into the JSON by hand must
        // not be able to point the dashboard at someone else's file.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sneaky.json");
        fs::write(
            &file,
            r#"{"method":"GET","path":"/x","status":200,"response":{},"source":"/etc/passwd"}"#,
        )
        .unwrap();

        let result = load_mocks_map(file.to_str().unwrap());
        assert_eq!(
            result.mocks["GET:/x"][0].source.as_deref(),
            Some(file.to_str().unwrap())
        );
    }

    // ------------------------------------------------------------------
    // response_file (#90)
    // ------------------------------------------------------------------

    /// A mocks directory containing the given files, parents created as needed.
    fn mocks_dir(files: &[(&str, &[u8])]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        dir
    }

    /// Load a directory with the default fixture size cap.
    fn load(dir: &TempDir) -> LoadResult {
        load_mocks_map_with_limit(dir.path().to_str().unwrap(), DEFAULT_MAX_RESPONSE_FILE)
    }

    #[test]
    fn response_file_bytes_are_read_at_load_time() {
        let dir = mocks_dir(&[
            (
                "export.json",
                br#"{"method":"GET","path":"/export","status":200,
                     "response_file":"fixtures/report.csv"}"#,
            ),
            ("fixtures/report.csv", b"id,name\n1,Alice\n"),
        ]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0);
        let mock = &result.mocks["GET:/export"][0];
        assert_eq!(
            mock.response_bytes.as_deref(),
            Some(b"id,name\n1,Alice\n".as_slice()),
            "request handling must never have to touch the disk"
        );
        assert_eq!(mock.response_file.as_deref(), Some("fixtures/report.csv"));
    }

    #[test]
    fn a_fixture_is_resolved_relative_to_its_own_mock_file() {
        // The same relative path under two directories has to resolve to two
        // different files, or a mocks tree stops being relocatable.
        let dir = mocks_dir(&[
            (
                "a/mock.json",
                br#"{"method":"GET","path":"/a","status":200,"response_file":"body.txt"}"#,
            ),
            ("a/body.txt", b"from a"),
            (
                "b/mock.json",
                br#"{"method":"GET","path":"/b","status":200,"response_file":"body.txt"}"#,
            ),
            ("b/body.txt", b"from b"),
        ]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0);
        assert_eq!(
            result.mocks["GET:/a"][0].response_bytes.as_deref(),
            Some(b"from a".as_slice())
        );
        assert_eq!(
            result.mocks["GET:/b"][0].response_bytes.as_deref(),
            Some(b"from b".as_slice())
        );
    }

    #[test]
    fn a_fixture_outside_the_mocks_root_is_refused() {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), b"password").unwrap();
        let dir = mocks_dir(&[(
            "leak.json",
            format!(
                r#"{{"method":"GET","path":"/leak","status":200,
                     "response_file":"../{}/secret"}}"#,
                outside.path().file_name().unwrap().to_str().unwrap()
            )
            .as_bytes(),
        )]);

        let error = load_error(&dir);
        assert!(
            error.contains("leak.json") && error.contains("outside the mocks root"),
            "the error has to name the mock file and the rule: {}",
            error
        );
    }

    #[test]
    fn an_absolute_fixture_path_outside_the_root_is_refused() {
        let dir = mocks_dir(&[(
            "passwd.json",
            br#"{"method":"GET","path":"/passwd","status":200,
                 "response_file":"/etc/hostname"}"#,
        )]);

        let error = load_error(&dir);
        assert!(error.contains("outside the mocks root"), "{}", error);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_the_root_is_refused() {
        // Containment is checked after canonicalization precisely so that a
        // symlink can't be used to walk out of a directory the operator meant
        // to be the boundary.
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, b"password").unwrap();

        let dir = mocks_dir(&[(
            "leak.json",
            br#"{"method":"GET","path":"/leak","status":200,
                 "response_file":"escape"}"#,
        )]);
        std::os::unix::fs::symlink(&secret, dir.path().join("escape")).unwrap();

        let error = load_error(&dir);
        assert!(error.contains("outside the mocks root"), "{}", error);
    }

    #[test]
    fn response_and_response_file_together_are_a_load_error() {
        let dir = mocks_dir(&[
            (
                "both.json",
                br#"{"method":"GET","path":"/both","status":200,
                     "response":{"ok":true},
                     "response_file":"fixtures/report.csv"}"#,
            ),
            ("fixtures/report.csv", b"id\n"),
        ]);

        let error = load_error(&dir);
        assert!(
            error.contains("both.json") && error.contains("response_file"),
            "{}",
            error
        );
    }

    #[test]
    fn neither_response_nor_response_file_still_means_a_null_response() {
        let dir = mocks_dir(&[(
            "empty.json",
            br#"{"method":"DELETE","path":"/users/1","status":204}"#,
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0);
        let mock = &result.mocks["DELETE:/users/1"][0];
        assert!(mock.response.is_null());
        assert!(mock.response_bytes.is_none());
    }

    #[test]
    fn a_missing_fixture_is_a_load_error_naming_the_mock() {
        let dir = mocks_dir(&[(
            "gone.json",
            br#"{"method":"GET","path":"/gone","status":200,
                 "response_file":"fixtures/nope.csv"}"#,
        )]);

        let error = load_error(&dir);
        assert!(
            error.contains("gone.json") && error.contains("nope.csv"),
            "{}",
            error
        );
    }

    #[test]
    fn an_oversized_fixture_skips_the_mock_instead_of_loading_it() {
        let dir = mocks_dir(&[
            (
                "big.json",
                br#"{"method":"GET","path":"/big","status":200,
                     "response_file":"fixtures/big.bin"}"#,
            ),
            ("fixtures/big.bin", &[0u8; 4096]),
        ]);

        let result = load_mocks_map_with_limit(dir.path().to_str().unwrap(), 1024);
        assert_eq!(result.errors.len(), 1);
        assert!(
            !result.mocks.contains_key("GET:/big"),
            "half a fixture is not a response"
        );

        // The same set loads once the cap is raised past the file.
        let result = load_mocks_map_with_limit(dir.path().to_str().unwrap(), 8192);
        assert_eq!(result.errors.len(), 0);
        assert_eq!(
            result.mocks["GET:/big"][0]
                .response_bytes
                .as_ref()
                .unwrap()
                .len(),
            4096
        );
    }

    #[test]
    fn a_zero_cap_disables_the_size_limit() {
        let dir = mocks_dir(&[
            (
                "big.json",
                br#"{"method":"GET","path":"/big","status":200,
                     "response_file":"fixtures/big.bin"}"#,
            ),
            ("fixtures/big.bin", &[7u8; 4096]),
        ]);

        let result = load_mocks_map_with_limit(dir.path().to_str().unwrap(), 0);
        assert_eq!(result.errors.len(), 0);
        assert!(result.mocks.contains_key("GET:/big"));
    }

    #[test]
    fn a_json_fixture_is_served_not_registered_as_a_mock() {
        // Fixtures have to live inside the mocks root, and the walk loads
        // every `.json` file it finds there. A fixture claimed by a mock is
        // neither registered nor counted as a file that failed to parse.
        let dir = mocks_dir(&[
            (
                "users.json",
                br#"{"method":"GET","path":"/users","status":200,
                     "response_file":"fixtures/users.json"}"#,
            ),
            ("fixtures/users.json", br#"{"users":[{"id":1}]}"#),
        ]);

        let result = load(&dir);
        assert_eq!(
            result.errors.len(),
            0,
            "a claimed fixture is not a broken mock"
        );
        assert_eq!(result.mocks.len(), 1);
        assert!(result.mocks.contains_key("GET:/users"));
    }

    #[test]
    fn an_unclaimed_json_file_that_is_not_a_mock_is_still_an_error() {
        // The fixture exemption is narrow: only files some mock actually
        // names. A typo'd mock file must not disappear into it.
        let dir = mocks_dir(&[("stray.json", br#"{"users":[{"id":1}]}"#)]);
        assert_eq!(load(&dir).errors.len(), 1);
    }

    #[test]
    fn a_sequence_step_can_declare_its_own_fixture() {
        let dir = mocks_dir(&[
            (
                "flaky.json",
                br#"{"method":"GET","path":"/flaky","status":200,"response":{"ok":true},
                     "sequence":[
                       {"status":503,"response":{"error":"unavailable"}},
                       {"status":200,"response_file":"fixtures/ok.csv","repeat":true}
                     ]}"#,
            ),
            ("fixtures/ok.csv", b"id\n1\n"),
        ]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0);
        let steps = result.mocks["GET:/flaky"][0].sequence.as_ref().unwrap();
        assert!(steps[0].response_bytes.is_none());
        assert_eq!(
            steps[1].response_bytes.as_deref(),
            Some(b"id\n1\n".as_slice())
        );
    }

    #[test]
    fn a_sequence_step_setting_both_bodies_is_a_load_error() {
        let dir = mocks_dir(&[
            (
                "flaky.json",
                br#"{"method":"GET","path":"/flaky","status":200,
                     "sequence":[
                       {"status":200,"response":{"ok":true},"response_file":"fixtures/ok.csv"}
                     ]}"#,
            ),
            ("fixtures/ok.csv", b"id\n"),
        ]);

        let error = load_error(&dir);
        assert!(error.contains("sequence step 0"), "{}", error);
    }

    #[test]
    fn response_bytes_written_into_a_mock_file_are_ignored() {
        // `response_bytes` is loader-owned; a mock file can't smuggle a body in.
        let dir = mocks_dir(&[(
            "sneaky.json",
            br#"{"method":"GET","path":"/x","status":200,"response":{"ok":true},
                 "response_bytes":[1,2,3]}"#,
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0);
        assert!(result.mocks["GET:/x"][0].response_bytes.is_none());
    }

    #[test]
    fn a_single_file_load_resolves_fixtures_beside_that_file() {
        let dir = mocks_dir(&[
            (
                "one.json",
                br#"{"method":"GET","path":"/one","status":200,"response_file":"body.txt"}"#,
            ),
            ("body.txt", b"hello"),
        ]);

        let file = dir.path().join("one.json");
        let result = load_mocks_map_with_limit(file.to_str().unwrap(), DEFAULT_MAX_RESPONSE_FILE);
        assert_eq!(result.errors.len(), 0);
        assert_eq!(
            result.mocks["GET:/one"][0].response_bytes.as_deref(),
            Some(b"hello".as_slice())
        );
    }

    /// Load `dir`, expecting exactly one failing file, and return its message.
    fn load_error(dir: &TempDir) -> String {
        let result = load(dir);
        let [error] = &result.errors[..] else {
            panic!(
                "expected exactly one load error, got {}: {:?}",
                result.errors.len(),
                result.errors
            );
        };
        assert_eq!(
            error.element, None,
            "these fixtures are single-object files, so the failure is whole-file"
        );
        assert!(
            result.mocks.is_empty(),
            "a mock that fails validation must not be registered"
        );
        error.message.clone()
    }

    #[test]
    fn load_failures_carry_the_file_and_the_reason() {
        let dir = mocks_dir(&[
            ("broken.json", br#"{"method":"GET""#),
            (
                "good.json",
                br#"{"method":"GET","path":"/good","status":200,"response":{}}"#,
            ),
        ]);

        let result = load(&dir);

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.files, 2, "both candidates were scanned");
        let error = &result.errors[0];
        assert_eq!(
            error.file.file_name().and_then(|f| f.to_str()),
            Some("broken.json")
        );
        assert!(
            error.message.contains("Failed to parse") && error.message.contains("broken.json"),
            "the message names the rule and the file: {}",
            error.message
        );
        assert!(result.mocks.contains_key("GET:/good"));
    }

    // ------------------------------------------------------------------
    // Multiple mocks in one file (#142)
    // ------------------------------------------------------------------

    #[test]
    fn an_array_file_registers_every_mock_in_it() {
        let dir = mocks_dir(&[(
            "orders.json",
            br#"[
                {"method":"GET","path":"/orders","status":200,"response":{"orders":[]}},
                {"method":"POST","path":"/orders","status":201,"response":{"ok":true}},
                {"method":"GET","path":"/orders/:id","status":200,"response":{"id":7}},
                {"method":"DELETE","path":"/orders/:id","status":204}
            ]"#,
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
        assert_eq!(result.mocks.len(), 4);
        assert!(result.mocks.contains_key("GET:/orders"));
        assert!(result.mocks.contains_key("POST:/orders"));
        assert!(result.mocks.contains_key("GET:/orders/:id"));
        assert!(result.mocks.contains_key("DELETE:/orders/:id"));

        // Each element is identified by its position in the file: the
        // `source` stays the plain path, and the index rides alongside it.
        for (index, key) in [
            "GET:/orders",
            "POST:/orders",
            "GET:/orders/:id",
            "DELETE:/orders/:id",
        ]
        .iter()
        .enumerate()
        {
            let mock = &result.mocks[*key][0];
            assert_eq!(mock.source_index, Some(index));
        }
        assert_eq!(
            result.mocks["GET:/orders"][0].source.as_deref(),
            Some(dir.path().join("orders.json").to_str().unwrap()),
            "`source` is the file to edit; the index lives in source_label"
        );
    }

    #[test]
    fn a_yaml_block_sequence_file_is_an_array_of_mocks() {
        // The realistic YAML array: block-sequence dashes, not flow syntax —
        // and a leading comment, one reason the shape is detected after
        // parsing rather than on the first character.
        let dir = mocks_dir(&[(
            "orders.yaml",
            b"# every endpoint of /orders, together\n- method: GET\n  path: /orders\n  status: 200\n  response:\n    orders: []\n- method: POST\n  path: /orders\n  status: 201\n  response:\n    ok: true\n",
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
        assert_eq!(result.mocks.len(), 2);
        assert!(result.mocks.contains_key("GET:/orders"));
        assert!(result.mocks.contains_key("POST:/orders"));
    }

    #[test]
    fn a_yaml_flow_sequence_file_is_also_an_array() {
        let dir = mocks_dir(&[(
            "ping.yaml",
            br#"[{method: GET, path: /ping, status: 200, response: {}}]"#,
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
        assert!(result.mocks.contains_key("GET:/ping"));
    }

    #[test]
    fn an_empty_array_file_registers_nothing() {
        let dir = mocks_dir(&[("empty.json", b"[]")]);
        let result = load(&dir);
        assert_eq!(result.errors.len(), 0);
        assert!(result.mocks.is_empty());
    }

    #[test]
    fn an_invalid_element_is_reported_with_its_index_and_its_siblings_load() {
        let dir = mocks_dir(&[(
            "orders.json",
            br#"[
                {"method":"GET","path":"/orders","status":200,"response":{"ok":true}},
                {"method":"","path":"/orders","status":201,"response":{"ok":true}},
                {"method":"DELETE","path":"/orders/1","status":204}
            ]"#,
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        let error = &result.errors[0];
        assert_eq!(
            error.file.file_name().and_then(|f| f.to_str()),
            Some("orders.json")
        );
        assert_eq!(error.element, Some(1), "the middle element failed");
        assert!(
            error.message.contains("orders.json#1"),
            "the message carries the positional file#index: {}",
            error.message
        );
        // The valid siblings still load, at their own positions.
        assert!(result.mocks.contains_key("GET:/orders"));
        assert!(result.mocks.contains_key("DELETE:/orders/1"));
    }

    #[test]
    fn a_broken_array_file_fails_as_a_whole() {
        // Not valid JSON at all: nothing is known about any mock inside, so
        // the file fails wholesale, like a single-object file always did.
        let dir = mocks_dir(&[("arr.json", b"[{\"method\":\"GET\"")]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].element, None);
        assert!(result.errors[0].message.contains("Failed to parse"));
        assert!(result.mocks.is_empty());
    }

    #[test]
    fn an_array_of_non_mocks_fails_as_a_whole() {
        // Valid JSON, but not an array of MockConfigs: the parse of the
        // array is a file-level failure naming the format and the file.
        let dir = mocks_dir(&[("numbers.json", b"[1,2,3]")]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].element, None);
        assert!(result.errors[0].message.contains("Failed to parse JSON"));
    }

    #[test]
    fn fixtures_claimed_from_array_elements_are_served_not_registered() {
        // The fixture exemption is about the file the walk found, not how
        // many mocks claim it — a .json fixture one array element references
        // must not be registered, and its bytes must reach the response.
        let dir = mocks_dir(&[
            (
                "downloads.json",
                br#"[
                    {"method":"GET","path":"/report","status":200,"response_file":"fixtures/report.json"},
                    {"method":"GET","path":"/notes","status":200,"response_file":"fixtures/notes.txt"}
                ]"#,
            ),
            ("fixtures/report.json", br#"{"rows":[1,2]}"#),
            ("fixtures/notes.txt", b"hello\n"),
        ]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
        assert_eq!(result.mocks.len(), 2, "the fixture is not a mock");
        assert_eq!(
            result.mocks["GET:/report"][0].response_bytes.as_deref(),
            Some(br#"{"rows":[1,2]}"#.as_slice())
        );
        assert_eq!(
            result.mocks["GET:/notes"][0].response_bytes.as_deref(),
            Some(b"hello\n".as_slice())
        );
    }

    #[test]
    fn a_misdeclared_fixture_from_an_array_element_is_an_element_error() {
        // Element 1's both-bodies mistake must not drag element 0 down, and
        // must name `#1`.
        let dir = mocks_dir(&[
            (
                "mixed.json",
                br#"[
                    {"method":"GET","path":"/ok","status":200,"response":{"ok":true}},
                    {"method":"GET","path":"/both","status":200,
                     "response":{"a":1},"response_file":"body.txt"}
                ]"#,
            ),
            ("body.txt", b"bytes"),
        ]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].element, Some(1));
        assert!(result.errors[0].message.contains("mixed.json#1"));
        assert!(result.mocks.contains_key("GET:/ok"));
    }

    #[test]
    fn array_elements_and_single_files_coexist_in_sorted_order() {
        // The load-order rule (#86) covers every mock in the walk: bucket
        // order stays a function of the file paths, with an array file's
        // elements landing together, in element order, at the position its
        // path sorts in.
        let dir = mocks_dir(&[
            (
                "a_users.json",
                br#"{"method":"GET","path":"/dup","status":200,"response":{"from":"a"}}"#,
            ),
            (
                "m_orders.json",
                br#"[
                    {"method":"GET","path":"/dup","status":200,"response":{"from":"element#0"}},
                    {"method":"GET","path":"/dup","status":200,"response":{"from":"element#1"}}
                ]"#,
            ),
            (
                "z_users.json",
                br#"{"method":"GET","path":"/dup","status":200,"response":{"from":"z"}}"#,
            ),
        ]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
        let order: Vec<String> = result.mocks["GET:/dup"]
            .iter()
            .map(|m| m.response["from"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            order,
            ["a", "element#0", "element#1", "z"],
            "the array file's elements load together, between the files their path sorts among"
        );
    }

    #[test]
    fn a_clean_array_file_is_cached_across_hot_reload_cycles() {
        // The hot-reload cache, with the outcome now holding several mocks:
        // a fully-loaded array file is neither re-read nor re-parsed on the
        // next cycle.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("good.json"),
            br#"[
                {"method":"GET","path":"/a","status":200,"response":{"ok":true}},
                {"method":"GET","path":"/b","status":200,"response":{"ok":true}}
            ]"#,
        )
        .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(first.changed, "the first cycle always loads");
        assert_eq!(first.result.errors.len(), 0, "{:?}", first.result.errors);

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(
            !second.changed,
            "the whole array outcome hit the cache; nothing else changed"
        );
        assert_eq!(second.result.mocks.len(), 2);
    }

    #[test]
    fn an_element_failure_is_retried_every_cycle_until_fixed() {
        // A file whose element fails validation isn't cached — the same
        // contract a file that fails wholesale has always had — so its fixed
        // element takes effect on the cycle after the fix, and while broken
        // the load error is reported every cycle.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("partial.json"),
            br#"[
                {"method":"GET","path":"/c","status":200,"response":{"ok":true}},
                {"method":"","path":"/d","status":200,"response":{"ok":true}}
            ]"#,
        )
        .unwrap();

        let mut cache = LoaderCache::new();
        let first = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert_eq!(first.result.errors.len(), 1);
        assert_eq!(first.result.errors[0].element, Some(1));
        assert!(first.result.mocks.contains_key("GET:/c"));

        let second = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(second.changed, "the failing element keeps the cycle hot");
        assert_eq!(second.result.errors.len(), 1);

        fs::write(
            dir.path().join("partial.json"),
            br#"[
                {"method":"GET","path":"/c","status":200,"response":{"ok":true}},
                {"method":"GET","path":"/d","status":200,"response":{"ok":true}}
            ]"#,
        )
        .unwrap();
        let third = load_mocks_map_hot_reload(dir.path().to_str().unwrap(), &mut cache);
        assert!(third.changed, "the fixed element changed");
        assert_eq!(third.result.errors.len(), 0, "{:?}", third.result.errors);
        assert!(third.result.mocks.contains_key("GET:/d"));
    }

    #[test]
    fn an_array_element_keeps_the_single_object_identity_carry_alive() {
        // The single-mock identity contract, restated next to the new
        // array identity: a single-object file's mock carries no element
        // index, so its identity is exactly what it was before the feature.
        let dir = mocks_dir(&[(
            "single.json",
            br#"{"method":"GET","path":"/users","status":200,"response":{"ok":true}}"#,
        )]);

        let result = load(&dir);
        let mock = &result.mocks["GET:/users"][0];
        assert_eq!(mock.source_index, None, "no index: not an array element");
        let identity = MockIdentity::of(mock, "GET:/users", 0);
        assert_eq!(
            identity,
            MockIdentity {
                key: "GET:/users".to_string(),
                origin: MockOrigin::Source(mock.source.clone().unwrap(), None),
            },
            "the identity a counter keyed on today is unchanged"
        );
        assert_eq!(identity.source(), Some(mock.source.clone().unwrap()));
        assert_eq!(
            identity.label(),
            format!("GET:/users @ {}", mock.source.as_deref().unwrap())
        );
    }

    #[test]
    fn two_sequence_elements_in_one_file_get_distinct_identities() {
        // The whole reason array elements need an index in their identity:
        // two sequence mocks in one file would otherwise collide on the
        // source path and share a counter.
        let dir = mocks_dir(&[(
            "flaky.json",
            br#"[
                {"method":"GET","path":"/flaky","status":200,
                 "sequence":[{"status":503,"response":{"step":1}},{"status":200,"response":{"step":2}}]},
                {"method":"GET","path":"/flaky","status":200,
                 "sequence":[{"status":500,"response":{"step":1}},{"status":201,"response":{"step":2}}]}
            ]"#,
        )]);

        let result = load(&dir);
        assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
        let bucket = &result.mocks["GET:/flaky"];
        assert_eq!(bucket.len(), 2);

        let identity_of = |index: usize| MockIdentity::of(&bucket[index], "GET:/flaky", index);
        let first = identity_of(0);
        let second = identity_of(1);
        assert_ne!(first, second, "two elements of one file are two mocks");

        let source = bucket[0].source.clone().unwrap();
        assert_eq!(
            first.source(),
            Some(format!("{}#0", source)),
            "the label names the element's place in the file"
        );
        assert_eq!(second.source(), Some(format!("{}#1", source)));
        assert_eq!(
            second.label(),
            format!("GET:/flaky @ {}", second.source().unwrap())
        );
    }
}
