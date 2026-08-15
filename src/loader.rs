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
pub fn load_mocks_map_with_limit(path: &str, max_response_file: u64) -> LoadResult {
    let path_obj = Path::new(path);

    if !path_obj.exists() {
        warn!("Mock path does not exist: {}", path);
        return LoadResult {
            mocks: HashMap::new(),
            errors: 1,
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

    // Pass one: read every candidate file, in a fixed order.
    let mut scanned: Vec<(PathBuf, Result<LoadedMock, String>)> = Vec::new();
    let mut errors: usize = 0;
    if path_obj.is_file() {
        scanned.push((
            path_obj.to_path_buf(),
            load_single_mock(path_obj, &root, max_response_file),
        ));
    } else if path_obj.is_dir() {
        collect_json_files(
            path_obj,
            &root,
            max_response_file,
            &mut scanned,
            &mut errors,
        );
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

    LoadResult { mocks, errors }
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
fn collect_json_files(
    dir: &Path,
    root: &Path,
    max_response_file: u64,
    scanned: &mut Vec<(PathBuf, Result<LoadedMock, String>)>,
    errors: &mut usize,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read directory {}: {}", dir.display(), e);
            *errors += 1;
            return;
        }
    };

    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for entry_path in paths {
        if entry_path.is_dir() {
            collect_json_files(&entry_path, root, max_response_file, scanned, errors);
        } else if entry_path.is_file()
            && entry_path.extension().and_then(|s| s.to_str()) == Some("json")
        {
            let outcome = load_single_mock(&entry_path, root, max_response_file);
            scanned.push((entry_path, outcome));
        }
    }
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
        let (resolved, bytes) = read_response_file(&file, path, root, max_response_file)?;
        mock.response_bytes = Some(bytes);
        fixtures.push(resolved);
    }

    for (index, step) in mock.sequence.iter_mut().flatten().enumerate() {
        step.response_bytes = None;
        let Some(file) = step.response_file.clone() else {
            continue;
        };
        reject_both_bodies(&step.response, &file, path)
            .map_err(|e| format!("{} (sequence step {})", e, index))?;
        let (resolved, bytes) = read_response_file(&file, path, root, max_response_file)
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

        let result = load_single_mock(&file_path, temp_dir.path(), DEFAULT_MAX_RESPONSE_FILE);
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
        collect_json_files(
            &root,
            &root,
            DEFAULT_MAX_RESPONSE_FILE,
            &mut scanned,
            &mut errors,
        );
        for (_, outcome) in scanned {
            if let Err(message) = outcome {
                messages.push(message);
            }
        }
        messages.join("\n")
    }
}
