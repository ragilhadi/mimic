use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockConfig {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub response: serde_json::Value,
}

pub type MockStore = Arc<HashMap<String, MockConfig>>;

pub fn create_mock_key(method: &str, path: &str) -> String {
    format!("{}:{}", method.to_uppercase(), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_mock_key() {
        assert_eq!(create_mock_key("get", "/users"), "GET:/users");
        assert_eq!(create_mock_key("POST", "/login"), "POST:/login");
        assert_eq!(
            create_mock_key("put", "/api/v1/products"),
            "PUT:/api/v1/products"
        );
    }

    #[test]
    fn test_create_mock_key_lowercase_conversion() {
        assert_eq!(create_mock_key("get", "/test"), "GET:/test");
        assert_eq!(create_mock_key("post", "/test"), "POST:/test");
        assert_eq!(create_mock_key("delete", "/test"), "DELETE:/test");
        assert_eq!(create_mock_key("patch", "/test"), "PATCH:/test");
    }

    #[test]
    fn test_create_mock_key_with_query_params() {
        assert_eq!(
            create_mock_key("GET", "/users?page=1&limit=10"),
            "GET:/users?page=1&limit=10"
        );
    }

    #[test]
    fn test_create_mock_key_with_special_chars() {
        assert_eq!(create_mock_key("GET", "/users/123"), "GET:/users/123");
        assert_eq!(create_mock_key("GET", "/api/v1/users"), "GET:/api/v1/users");
        assert_eq!(create_mock_key("GET", "/users-list"), "GET:/users-list");
    }

    #[test]
    fn test_mock_config_serialization() {
        let json = r#"{
            "method": "GET",
            "path": "/test",
            "status": 200,
            "response": {"message": "success"}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "GET");
        assert_eq!(config.path, "/test");
        assert_eq!(config.status, 200);
    }

    #[test]
    fn test_mock_config_deserialization() {
        let config = MockConfig {
            method: "POST".to_string(),
            path: "/api/users".to_string(),
            status: 201,
            response: serde_json::json!({"id": 1, "name": "Alice"}),
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MockConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.method, "POST");
        assert_eq!(deserialized.path, "/api/users");
        assert_eq!(deserialized.status, 201);
        assert_eq!(deserialized.response["id"], 1);
    }

    #[test]
    fn test_mock_config_with_null_response() {
        let json = r#"{
            "method": "DELETE",
            "path": "/users/1",
            "status": 204,
            "response": null
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "DELETE");
        assert_eq!(config.status, 204);
        assert!(config.response.is_null());
    }

    #[test]
    fn test_mock_config_with_array_response() {
        let json = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": [{"id": 1}, {"id": 2}]
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert!(config.response.is_array());
        assert_eq!(config.response.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_mock_config_with_nested_response() {
        let json = r#"{
            "method": "GET",
            "path": "/api/data",
            "status": 200,
            "response": {
                "data": {
                    "user": {
                        "id": 1,
                        "profile": {
                            "name": "Alice"
                        }
                    }
                }
            }
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.response["data"]["user"]["profile"]["name"], "Alice");
    }

    #[test]
    fn test_mock_config_clone() {
        let config = MockConfig {
            method: "GET".to_string(),
            path: "/test".to_string(),
            status: 200,
            response: serde_json::json!({"test": true}),
        };

        let cloned = config.clone();
        assert_eq!(cloned.method, config.method);
        assert_eq!(cloned.path, config.path);
        assert_eq!(cloned.status, config.status);
    }

    #[test]
    fn test_mock_store_operations() {
        let mut map = HashMap::new();
        map.insert(
            "GET:/test".to_string(),
            MockConfig {
                method: "GET".to_string(),
                path: "/test".to_string(),
                status: 200,
                response: serde_json::json!({}),
            },
        );

        let store: MockStore = Arc::new(map);
        assert_eq!(store.len(), 1);
        assert!(store.contains_key("GET:/test"));
        assert!(!store.contains_key("POST:/test"));
    }
}
