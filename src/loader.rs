use crate::types::{create_mock_key, MockConfig, MockStore};
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

/// Result of loading mock configurations, including any errors encountered.
pub struct LoadResult {
    pub mocks: HashMap<String, Vec<MockConfig>>,
    pub errors: usize,
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
struct CachedMock {
    fingerprint: FileFingerprint,
    fixture_fingerprints: Vec<(PathBuf, FileFingerprint)>,
    outcome: LoadedMock,
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
/// The fixture list is what lets the directory walk tell a fixture apart from a
/// mock. Fixtures live inside the mocks root — the containment check requires
/// it — so a `.json` fixture would otherwise be picked up by the walk and
/// reported as a mock file that failed to parse.
#[derive(Clone)]
struct LoadedMock {
    mock: MockConfig,
    /// Canonical paths of the files this mock serves its bodies from.
    fixtures: Vec<PathBuf>,
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
///     path (str): Path to directory containing JSON mock files or a single JSON file.
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
                errors: 1,
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
    let mut scanned: Vec<(PathBuf, Result<LoadedMock, String>)> = Vec::new();
    let mut errors: usize = 0;
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
        collect_json_files(path_obj, &root, max_response_file, &mut state, 0);
    }

    let before = cache.files.len();
    cache.files.retain(|file, _| seen.contains(file));
    if cache.files.len() != before {
        changed = true;
    }

    // Every file claimed as a fixture by a mock that loaded. A fixture is
    // served, never registered — including when it is itself valid JSON.
    let fixtures: HashSet<&PathBuf> = scanned
        .iter()
        .filter_map(|(_, outcome)| outcome.as_ref().ok())
        .flat_map(|loaded| loaded.fixtures.iter())
        .collect();

    // Pass two: register what parsed, minus the fixtures.
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
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
            continue;
        }

        match outcome {
            Ok(loaded) => {
                let mock = loaded.mock.clone();
                let key = create_mock_key(&mock.method, &mock.path);
                debug!("Loaded mock: {} -> {}", key, file.display());
                let entry = mocks.entry(key).or_default();
                if !entry.is_empty() {
                    warn!(
                        "Multiple mocks registered for {} {}: {} total (file: {})",
                        mock.method,
                        mock.path,
                        entry.len() + 1,
                        file.display()
                    );
                }
                entry.push(mock);
            }
            Err(e) => {
                warn!("Failed to load mock file {}: {}", file.display(), e);
                errors += 1;
            }
        }
    }

    ReloadOutcome {
        result: LoadResult { mocks, errors },
        // A file that fails to parse isn't cached (see
        // `load_single_mock_maybe_cached`), so it's retried — and `errors`
        // stays nonzero — every cycle until it's fixed or removed. Treating
        // that as "changed" every time matches `apply_reload`'s existing
        // carry-forward behavior, which already re-runs on every such cycle.
        changed: changed || errors > 0,
    }
}

/// Loads mock configurations from a directory or file.
///
/// Args:
///     path (str): Path to directory containing JSON mock files or a single JSON file.
///
/// Returns:
///     MockStore: Thread-safe HashMap of mock configurations keyed by "METHOD:PATH".
pub fn load_mocks(path: &str) -> MockStore {
    let result = load_mocks_map(path);
    Arc::new(RwLock::new(result.mocks))
}

/// Recursively collects and loads all JSON mock files from a directory tree.
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
/// How many directory levels the walk follows before giving up and reporting
/// a cycle rather than continuing silently.
///
/// This is a backstop for whatever `visited` can't catch — a dangling or
/// otherwise uncanonicalizable symlink — not a limit any real mocks tree
/// should come near. `visited`, keyed by canonical path, is what actually
/// stops an ordinary symlink cycle after one extra level.
const MAX_WALK_DEPTH: usize = 64;

/// Everything one level of [`collect_json_files`]'s recursion needs, bundled
/// so adding a cross-cycle concern (the cache, the change flag) doesn't mean
/// adding another positional parameter to every recursive call.
struct ScanState<'a> {
    scanned: &'a mut Vec<(PathBuf, Result<LoadedMock, String>)>,
    errors: &'a mut usize,
    visited: &'a mut HashSet<PathBuf>,
    cache: &'a mut LoaderCache,
    changed: &'a mut bool,
    seen: &'a mut HashSet<PathBuf>,
}

fn collect_json_files(
    dir: &Path,
    root: &Path,
    max_response_file: u64,
    state: &mut ScanState,
    depth: usize,
) {
    if depth > MAX_WALK_DEPTH {
        error!(
            "Mocks directory walk exceeded {} levels at {}; stopping here \
             (an unresolvable symlink cycle?)",
            MAX_WALK_DEPTH,
            dir.display()
        );
        *state.errors += 1;
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read directory {}: {}", dir.display(), e);
            *state.errors += 1;
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
            collect_json_files(&entry_path, root, max_response_file, state, depth + 1);
        } else if entry_path.is_file()
            && entry_path.extension().and_then(|s| s.to_str()) == Some("json")
        {
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
) -> Result<LoadedMock, String> {
    let fingerprint = fs::metadata(path)
        .ok()
        .and_then(|m| FileFingerprint::of(&m));

    if let Some(fp) = fingerprint {
        if let Some(cached) = cache.files.get(path) {
            if cached.fingerprint == fp && fixtures_unchanged(&cached.fixture_fingerprints) {
                return Ok(cached.outcome.clone());
            }
        }
    }

    *changed = true;
    let outcome = load_single_mock(path, root, max_response_file, &mut cache.fixtures);

    match (&outcome, fingerprint) {
        (Ok(loaded), Some(fp)) => {
            let fixture_fingerprints = loaded
                .fixtures
                .iter()
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
                    outcome: loaded.clone(),
                },
            );
        }
        _ => {
            // Parse failure, or a `stat` that raced with a delete: don't cache
            // it, so the next cycle retries rather than serving a stale
            // "still broken" verdict forever.
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

/// Loads a single mock configuration from a JSON file.
///
/// `root` is the mocks root a `response_file` may not escape; `max_response_file`
/// caps how large one of those files may be.
///
/// Returns the parsed mock along with the fixture files it claims, or an error
/// message naming the offending file.
fn load_single_mock(
    path: &Path,
    root: &Path,
    max_response_file: u64,
    fixture_cache: &mut HashMap<PathBuf, (FileFingerprint, Bytes)>,
) -> Result<LoadedMock, String> {
    let contents = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read file {}: {}", path.display(), e))?;

    let mut mock: MockConfig = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse JSON in {}: {}", path.display(), e))?;

    // The file this mock came from is the answer to "where do I go to change
    // this?", so it's recorded here rather than dropped after the parse.
    // Assigned unconditionally: `source` is loader-owned, and a value written
    // into the mock JSON by hand must not be able to misattribute a mock.
    mock.source = Some(path.display().to_string());

    // Validate required fields
    if mock.method.is_empty() {
        return Err(format!("Empty method in {}", path.display()));
    }
    if mock.path.is_empty() {
        return Err(format!("Empty path in {}", path.display()));
    }

    // Response bodies read from disk. Also loader-owned: whatever a mock file
    // says about `response_bytes` is dropped by serde before we get here.
    mock.response_bytes = None;
    let mut fixtures = Vec::new();

    if let Some(file) = mock.response_file.clone() {
        reject_both_bodies(&mock.response, &file, path)?;
        let (resolved, bytes) =
            read_response_file(&file, path, root, max_response_file, fixture_cache)?;
        mock.response_bytes = Some(bytes);
        fixtures.push(resolved);
    }

    if !crate::types::is_valid_status(mock.status) {
        warn!(
            "{}: status {} is outside 100-599 and will be served as 200 OK",
            path.display(),
            mock.status
        );
    }

    for (index, step) in mock.sequence.iter_mut().flatten().enumerate() {
        step.response_bytes = None;
        if !crate::types::is_valid_status(step.status) {
            warn!(
                "{}: sequence step {} has status {}, outside 100-599, and will be served as 200 OK",
                path.display(),
                index,
                step.status
            );
        }
        let Some(file) = step.response_file.clone() else {
            continue;
        };
        reject_both_bodies(&step.response, &file, path)
            .map_err(|e| format!("{} (sequence step {})", e, index))?;
        let (resolved, bytes) =
            read_response_file(&file, path, root, max_response_file, fixture_cache)
                .map_err(|e| format!("{} (sequence step {})", e, index))?;
        step.response_bytes = Some(bytes);
        fixtures.push(resolved);
    }

    Ok(LoadedMock { mock, fixtures })
}

/// `response` and `response_file` are two answers to one question, so a mock
/// that gives both is rejected rather than having one silently win.
fn reject_both_bodies(
    response: &serde_json::Value,
    file: &str,
    mock_path: &Path,
) -> Result<(), String> {
    if response.is_null() {
        return Ok(());
    }
    Err(format!(
        "{} sets both `response` and `response_file` ({}); use one or the other",
        mock_path.display(),
        file
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
fn read_response_file(
    file: &str,
    mock_path: &Path,
    root: &Path,
    max_response_file: u64,
    fixture_cache: &mut HashMap<PathBuf, (FileFingerprint, Bytes)>,
) -> Result<(PathBuf, Bytes), String> {
    let base = mock_path.parent().unwrap_or(Path::new("."));
    let candidate = base.join(file);

    let resolved = candidate.canonicalize().map_err(|e| {
        format!(
            "{}: cannot read response_file '{}' ({}): {}",
            mock_path.display(),
            file,
            candidate.display(),
            e
        )
    })?;

    if !resolved.starts_with(root) {
        return Err(format!(
            "{}: response_file '{}' resolves to {}, which is outside the mocks root {}",
            mock_path.display(),
            file,
            resolved.display(),
            root.display()
        ));
    }

    let metadata = fs::metadata(&resolved).map_err(|e| {
        format!(
            "{}: cannot stat response_file '{}': {}",
            mock_path.display(),
            file,
            e
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "{}: response_file '{}' is not a regular file",
            mock_path.display(),
            file
        ));
    }
    // Checked against the file's size before reading, so an oversized fixture
    // costs a stat rather than a 2 GB allocation.
    if max_response_file > 0 && metadata.len() > max_response_file {
        return Err(format!(
            "{}: response_file '{}' is {} bytes, over the {}-byte {} limit; skipping this mock",
            mock_path.display(),
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

        let bytes = fs::read(&resolved).map_err(|e| {
            format!(
                "{}: failed to read response_file '{}': {}",
                mock_path.display(),
                file,
                e
            )
        })?;
        let bytes = Bytes::from(bytes);

        debug!(
            "Loaded response_file {} ({} bytes) for {}",
            resolved.display(),
            bytes.len(),
            mock_path.display()
        );

        fixture_cache.insert(resolved.clone(), (fingerprint, bytes.clone()));
        return Ok((resolved, bytes));
    }

    // Metadata lacks a usable modified time (some platforms/filesystems):
    // caching would be unsound, so just read it every time, as before.
    let bytes = fs::read(&resolved).map_err(|e| {
        format!(
            "{}: failed to read response_file '{}': {}",
            mock_path.display(),
            file,
            e
        )
    })?;

    debug!(
        "Loaded response_file {} ({} bytes) for {}",
        resolved.display(),
        bytes.len(),
        mock_path.display()
    );

    Ok((resolved, Bytes::from(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(result.errors, 0);
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

        assert_eq!(result.errors, 0);
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

        File::create(&file)
            .unwrap()
            .write_all(br#"{"method":"GET","path":"/users","status":201,"response":[]}"#)
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
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn test_load_mocks_empty_path() {
        let temp_dir = TempDir::new().unwrap();
        let result = load_mocks_map(temp_dir.path().to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn test_load_mocks_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("invalid.json");

        let mut file = File::create(&file_path).unwrap();
        file.write_all(b"{invalid json}").unwrap();

        let result = load_mocks_map(temp_dir.path().to_str().unwrap());
        assert_eq!(result.mocks.len(), 0);
        assert_eq!(result.errors, 1);
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
        assert_eq!(result.errors, 1);
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
        assert_eq!(result.errors, 1);
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
        assert!(result.is_ok());

        let mock_config = result.unwrap().mock;
        assert_eq!(mock_config.method, "POST");
        assert_eq!(mock_config.path, "/api/test");
        assert_eq!(mock_config.status, 201);
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
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
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
        assert!(result.errors > 0);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 1);
        assert!(
            !result.mocks.contains_key("GET:/big"),
            "half a fixture is not a response"
        );

        // The same set loads once the cap is raised past the file.
        let result = load_mocks_map_with_limit(dir.path().to_str().unwrap(), 8192);
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0, "a claimed fixture is not a broken mock");
        assert_eq!(result.mocks.len(), 1);
        assert!(result.mocks.contains_key("GET:/users"));
    }

    #[test]
    fn an_unclaimed_json_file_that_is_not_a_mock_is_still_an_error() {
        // The fixture exemption is narrow: only files some mock actually
        // names. A typo'd mock file must not disappear into it.
        let dir = mocks_dir(&[("stray.json", br#"{"users":[{"id":1}]}"#)]);
        assert_eq!(load(&dir).errors, 1);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
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
        assert_eq!(result.errors, 0);
        assert_eq!(
            result.mocks["GET:/one"][0].response_bytes.as_deref(),
            Some(b"hello".as_slice())
        );
    }

    /// Load `dir`, expecting exactly one failing file, and return its message.
    fn load_error(dir: &TempDir) -> String {
        let result = load(dir);
        assert_eq!(result.errors, 1, "expected exactly one load error");
        assert!(
            result.mocks.is_empty(),
            "a mock that fails validation must not be registered"
        );

        // The message the loader logs is the message `load_single_mock`
        // returns, so assert on that rather than on captured log output.
        let root = dir.path().canonicalize().unwrap();
        let mut messages = Vec::new();
        let mut scanned = Vec::new();
        let mut errors = 0;
        let mut visited = HashSet::new();
        visited.insert(root.clone());
        let mut cache = LoaderCache::new();
        let mut changed = false;
        let mut seen = HashSet::new();
        let mut state = ScanState {
            scanned: &mut scanned,
            errors: &mut errors,
            visited: &mut visited,
            cache: &mut cache,
            changed: &mut changed,
            seen: &mut seen,
        };
        collect_json_files(&root, &root, DEFAULT_MAX_RESPONSE_FILE, &mut state, 0);
        for (_, outcome) in scanned {
            if let Err(message) = outcome {
                messages.push(message);
            }
        }
        messages.join("\n")
    }
}
