use crate::types::{create_mock_key, MockConfig, MockStore};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, error, warn};

/// Loads mock configurations from a directory or file.
///
/// Args:
///     path (str): Path to directory containing JSON mock files or a single JSON file.
///
/// Returns:
///     MockStore: Thread-safe HashMap of mock configurations keyed by "METHOD:PATH".
pub fn load_mocks(path: &str) -> MockStore {
    let path_obj = Path::new(path);

    if !path_obj.exists() {
        warn!("Mock path does not exist: {}", path);
        return Arc::new(HashMap::new());
    }

    let mut mocks = HashMap::new();

    if path_obj.is_file() {
        // Load single file
        if let Ok(mock) = load_single_mock(path_obj) {
            let key = create_mock_key(&mock.method, &mock.path);
            mocks.insert(key, mock);
        }
    } else if path_obj.is_dir() {
        // Load all JSON files from directory
        match fs::read_dir(path_obj) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let entry_path = entry.path();
                    if entry_path.is_file()
                        && (entry_path.extension().and_then(|s| s.to_str()) == Some("json"))
                    {
                        if let Ok(mock) = load_single_mock(&entry_path) {
                            let key = create_mock_key(&mock.method, &mock.path);
                            debug!("Loaded mock: {} -> {}", key, entry_path.display());
                            mocks.insert(key, mock);
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to read directory {}: {}", path, e);
            }
        }
    }

    Arc::new(mocks)
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

    let mock: MockConfig = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse JSON in {}: {}", path.display(), e))?;

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
    use std::fs::File;
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

        let store = load_mocks(dir_path.to_str().unwrap());
        assert_eq!(store.len(), 2);
        assert!(store.contains_key("GET:/users"));
        assert!(store.contains_key("POST:/login"));
    }

    #[test]
    fn test_load_mocks_nonexistent_directory() {
        let store = load_mocks("/nonexistent/path");
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_empty_path() {
        let temp_dir = TempDir::new().unwrap();
        let store = load_mocks(temp_dir.path().to_str().unwrap());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_ignores_non_json_files() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create a non-JSON file
        let txt_file = dir_path.join("readme.txt");
        let mut file = File::create(&txt_file).unwrap();
        file.write_all(b"This is not JSON").unwrap();

        let store = load_mocks(dir_path.to_str().unwrap());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("invalid.json");

        let mut file = File::create(&file_path).unwrap();
        file.write_all(b"{invalid json}").unwrap();

        let store = load_mocks(temp_dir.path().to_str().unwrap());
        assert_eq!(store.len(), 0);
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

        let store = load_mocks(temp_dir.path().to_str().unwrap());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_file_not_directory() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("not_a_dir.txt");
        File::create(&file_path).unwrap();

        let store = load_mocks(file_path.to_str().unwrap());
        assert_eq!(store.len(), 0);
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

        let store = load_mocks(dir_path.to_str().unwrap());
        assert_eq!(store.len(), 5);
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
        };

        let key = create_mock_key(&mock.method, &mock.path);
        assert_eq!(key, "PUT:/api/users/1");
    }
}
