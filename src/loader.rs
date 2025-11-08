use crate::types::{create_mock_key, MockConfig, MockStore};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tracing::{error, info, warn};

#[cfg(test)]
use serde_json::json;


pub fn load_mocks(mocks_dir: &str) -> MockStore {
    let mut mocks = HashMap::new();
    let path = Path::new(mocks_dir);

    if !path.exists() {
        error!("Mocks directory does not exist: {}", mocks_dir);
        return Arc::new(mocks);
    }

    if !path.is_dir() {
        error!("Mocks path is not a directory: {}", mocks_dir);
        return Arc::new(mocks);
    }

    info!("Loading mocks from directory: {}", mocks_dir);

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read mocks directory: {}", e);
            return Arc::new(mocks);
        }
    };

    let mut loaded_count = 0;
    let mut error_count = 0;

    for entry in entries.flatten() {
        let file_path = entry.path();

        // Only process .json files
        if file_path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        match load_single_mock(&file_path) {
            Ok(mock) => {
                let key = create_mock_key(&mock.method, &mock.path);
                info!(
                    "Loaded mock: {} {} -> {} (from {:?})",
                    mock.method, mock.path, mock.status, file_path.file_name()
                );
                mocks.insert(key, mock);
                loaded_count += 1;
            }
            Err(e) => {
                warn!("Failed to load mock from {:?}: {}", file_path, e);
                error_count += 1;
            }
        }
    }

    info!(
        "Mock loading complete: {} loaded, {} errors",
        loaded_count, error_count
    );

    Arc::new(mocks)
}

fn load_single_mock(file_path: &Path) -> Result<MockConfig, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(file_path)?;
    let mock: MockConfig = serde_json::from_str(&content)?;

    // Validate required fields
    if mock.method.is_empty() {
        return Err("Mock method cannot be empty".into());
    }

    if mock.path.is_empty() {
        return Err("Mock path cannot be empty".into());
    }

    Ok(mock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;
    #[test]
    fn test_load_mocks_from_directory() {
        let temp_dir = TempDir::new().unwrap();
        let mocks_path = temp_dir.path().to_str().unwrap();

        // Create test mock files
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

        let mut file1 = File::create(temp_dir.path().join("get_users.json")).unwrap();
        file1.write_all(mock1.as_bytes()).unwrap();

        let mut file2 = File::create(temp_dir.path().join("post_login.json")).unwrap();
        file2.write_all(mock2.as_bytes()).unwrap();

        // Load mocks
        let store = load_mocks(mocks_path);

        assert_eq!(store.len(), 2);
        assert!(store.contains_key("GET:/users"));
        assert!(store.contains_key("POST:/login"));
    }

    #[test]
    fn test_load_mocks_ignores_non_json_files() {
        let temp_dir = TempDir::new().unwrap();
        let mocks_path = temp_dir.path().to_str().unwrap();

        let mock = r#"{
            "method": "GET",
            "path": "/test",
            "status": 200,
            "response": {}
        }"#;

        // Create JSON file
        let mut json_file = File::create(temp_dir.path().join("test.json")).unwrap();
        json_file.write_all(mock.as_bytes()).unwrap();

        // Create non-JSON file
        let mut txt_file = File::create(temp_dir.path().join("readme.txt")).unwrap();
        txt_file.write_all(b"This should be ignored").unwrap();

        let store = load_mocks(mocks_path);

        assert_eq!(store.len(), 1);
        assert!(store.contains_key("GET:/test"));
    }

    #[test]
    fn test_load_mocks_nonexistent_directory() {
        let store = load_mocks("/nonexistent/path");
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let mocks_path = temp_dir.path().to_str().unwrap();

        // Create invalid JSON file
        let mut invalid_file = File::create(temp_dir.path().join("invalid.json")).unwrap();
        invalid_file.write_all(b"{ invalid json }").unwrap();

        // Should handle error gracefully
        let store = load_mocks(mocks_path);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_empty_method() {
        let temp_dir = TempDir::new().unwrap();
        let mocks_path = temp_dir.path().to_str().unwrap();

        let mock = r#"{
            "method": "",
            "path": "/test",
            "status": 200,
            "response": {}
        }"#;

        let mut file = File::create(temp_dir.path().join("empty_method.json")).unwrap();
        file.write_all(mock.as_bytes()).unwrap();

        let store = load_mocks(mocks_path);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_empty_path() {
        let temp_dir = TempDir::new().unwrap();
        let mocks_path = temp_dir.path().to_str().unwrap();

        let mock = r#"{
            "method": "GET",
            "path": "",
            "status": 200,
            "response": {}
        }"#;

        let mut file = File::create(temp_dir.path().join("empty_path.json")).unwrap();
        file.write_all(mock.as_bytes()).unwrap();

        let store = load_mocks(mocks_path);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_mocks_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let mocks_path = temp_dir.path().to_str().unwrap();

        // Create multiple valid mock files
        for i in 1..=5 {
            let mock = format!(
                r#"{{
                    "method": "GET",
                    "path": "/api/v{}",
                    "status": 200,
                    "response": {{"version": {}}}
                }}"#,
                i, i
            );
            let mut file = File::create(temp_dir.path().join(format!("mock{}.json", i))).unwrap();
            file.write_all(mock.as_bytes()).unwrap();
        }

        let store = load_mocks(mocks_path);
        assert_eq!(store.len(), 5);
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
    fn test_create_mock_key_from_config() {
        let mock = MockConfig {
            method: "PUT".to_string(),
            path: "/api/users/1".to_string(),
            status: 200,
            response: json!({}),
        };

        let key = create_mock_key(&mock.method, &mock.path);
        assert_eq!(key, "PUT:/api/users/1");
    }
}
