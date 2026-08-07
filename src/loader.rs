use crate::types::{create_mock_key, MockConfig, MockStore};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

/// Result of loading mock configurations, including any errors encountered.
pub struct LoadResult {
    pub mocks: HashMap<String, Vec<MockConfig>>,
    pub errors: usize,
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
    let path_obj = Path::new(path);

    if !path_obj.exists() {
        warn!("Mock path does not exist: {}", path);
        return LoadResult {
            mocks: HashMap::new(),
            errors: 1,
        };
    }

    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    let mut errors: usize = 0;

    if path_obj.is_file() {
        // Load single file
        match load_single_mock(path_obj) {
            Ok(mock) => {
                let key = create_mock_key(&mock.method, &mock.path);
                let entry = mocks.entry(key).or_default();
                if !entry.is_empty() {
                    warn!(
                        "Multiple mocks registered for {} {}: {} total",
                        mock.method,
                        mock.path,
                        entry.len() + 1
                    );
                }
                entry.push(mock);
            }
            Err(e) => {
                warn!("Failed to load mock file {}: {}", path, e);
                errors += 1;
            }
        }
    } else if path_obj.is_dir() {
        // Load all JSON files from directory tree (recursive)
        collect_json_files(path_obj, &mut mocks, &mut errors);
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
fn collect_json_files(
    dir: &Path,
    mocks: &mut HashMap<String, Vec<MockConfig>>,
    errors: &mut usize,
) {
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    collect_json_files(&entry_path, mocks, errors);
                } else if entry_path.is_file()
                    && entry_path.extension().and_then(|s| s.to_str()) == Some("json")
                {
                    match load_single_mock(&entry_path) {
                        Ok(mock) => {
                            let key = create_mock_key(&mock.method, &mock.path);
                            debug!("Loaded mock: {} -> {}", key, entry_path.display());
                            let entry = mocks.entry(key).or_default();
                            if !entry.is_empty() {
                                warn!(
                                    "Multiple mocks registered for {} {}: {} total (file: {})",
                                    mock.method,
                                    mock.path,
                                    entry.len() + 1,
                                    entry_path.display()
                                );
                            }
                            entry.push(mock);
                        }
                        Err(e) => {
                            warn!("Failed to load mock file {}: {}", entry_path.display(), e);
                            *errors += 1;
                        }
                    }
                }
            }
        }
        Err(e) => {
            error!("Failed to read directory {}: {}", dir.display(), e);
            *errors += 1;
        }
    }
}

/// Loads a single mock configuration from a JSON file.
///
/// Args:
///     path (Path): Path to the JSON file.
///
/// Returns:
///     Result<MockConfig, String>: Parsed mock configuration or error message.
fn load_single_mock(path: &Path) -> Result<MockConfig, String> {
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

    Ok(mock)
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

        let result = load_single_mock(&file_path);
        assert!(result.is_ok());

        let mock_config = result.unwrap();
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
}
