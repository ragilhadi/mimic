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
        assert_eq!(create_mock_key("put", "/api/v1/products"), "PUT:/api/v1/products");
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
}
