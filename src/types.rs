use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ============================================================================
// Sensitive Header Handling
// ============================================================================

/// Headers whose values must never leave the process — neither stored in the
/// request log nor interpolated into a mock response by templating.
///
/// Defined here (rather than in one consumer) so `handler.rs` and
/// `template.rs` can't drift apart on what counts as a secret.
pub const SENSITIVE_HEADERS: &[&str] = &["authorization", "cookie", "set-cookie"];

/// True if `name` names a header carrying credentials. Case-insensitive, so
/// it works on both already-normalized and raw header names.
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

// ============================================================================
// Query Parameter Matching Types
// ============================================================================

/// Matcher for URL query parameters
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryParamMatcher {
    /// Parameters that must match
    #[serde(default)]
    pub params: HashMap<String, QueryParamValue>,

    /// If true, request must not have extra params beyond those specified
    #[serde(default)]
    pub strict: bool,
}

/// Value matcher for a single query parameter
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QueryParamValue {
    /// Exact value match (simple string)
    Exact(String),

    /// Pattern-based matching
    Pattern(QueryParamPattern),
}

/// Pattern matching options for query parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryParamPattern {
    /// Regex pattern match
    Regex(String),
    /// Any value (parameter must exist)
    Any,
}

// ============================================================================
// Header Matching Types
// ============================================================================

/// Matcher for HTTP headers
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeaderMatcher {
    /// Headers that must be present and match
    #[serde(default)]
    pub required: HashMap<String, HeaderValue>,

    /// Headers that must NOT be present
    #[serde(default)]
    pub forbidden: Vec<String>,

    /// If true, request must not have extra headers beyond required
    #[serde(default)]
    pub strict: bool,
}

/// Value matcher for a single header
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HeaderValue {
    /// Exact value match (simple string)
    Exact(String),

    /// Pattern-based matching
    Pattern(HeaderPattern),
}

/// Pattern matching options for headers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPattern {
    /// Regex pattern match
    Regex(String),
    /// Any value (header must exist)
    Any,
    /// Starts with prefix
    Prefix(String),
    /// Contains substring
    Contains(String),
}

// ============================================================================
// Body Matching Types
// ============================================================================

/// Matcher for request body
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BodyMatcher {
    /// Match JSON body
    Json(JsonBodyMatcher),

    /// Match text/plain body
    Text(TextBodyMatcher),

    /// Match form data (application/x-www-form-urlencoded)
    Form(FormBodyMatcher),

    /// Match any body (just check presence)
    Any,

    /// Match empty body
    Empty,
}

/// JSON body matcher with multiple strategies
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JsonBodyMatcher {
    /// Exact JSON match (deep equality)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<serde_json::Value>,

    /// Partial match: specified fields must match, others ignored
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<serde_json::Value>,

    /// If true with partial match, reject extra fields
    #[serde(default)]
    pub strict: bool,
}

/// Text body matcher
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TextBodyMatcher {
    /// Exact text match
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<String>,

    /// Contains substring
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,

    /// Regex pattern
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// Form data matcher
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FormBodyMatcher {
    /// Form fields that must match
    #[serde(default)]
    pub fields: HashMap<String, String>,

    /// If true, reject extra fields
    #[serde(default)]
    pub strict: bool,
}

// ============================================================================
// Main Mock Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockConfig {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub response: serde_json::Value,

    #[serde(default = "default_consume_body")]
    pub consume_body: bool,

    /// Optional query parameter matcher
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_params: Option<QueryParamMatcher>,

    /// Optional header matcher
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderMatcher>,

    /// Optional body matcher
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyMatcher>,

    /// Optional custom response headers (names are case-insensitive)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<HashMap<String, String>>,

    /// Optional response delay: fixed ms or a {min, max} range
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<DelayConfig>,

    /// Optional stateful response sequence (one step consumed per request)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Vec<SequenceStep>>,
}

/// Response delay configuration: a fixed duration or a random range
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DelayConfig {
    Fixed(u64),
    Range { min: u64, max: u64 },
}

impl DelayConfig {
    /// Resolve the configured delay to a concrete duration in milliseconds
    pub fn resolve(&self) -> u64 {
        match self {
            DelayConfig::Fixed(ms) => *ms,
            DelayConfig::Range { min, max } => {
                if max <= min {
                    *min
                } else {
                    rand::rng().random_range(*min..=*max)
                }
            }
        }
    }
}

/// One step in a stateful response sequence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceStep {
    pub status: u16,
    pub response: serde_json::Value,

    /// Optional delay applied before returning this step's response
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,

    /// If true, the sequence stops advancing at this step
    #[serde(default)]
    pub repeat: bool,
}

fn default_consume_body() -> bool {
    false
}

/// Check if a mock has any advanced matchers
impl MockConfig {
    #[allow(dead_code)]
    pub fn has_advanced_matchers(&self) -> bool {
        self.query_params.is_some() || self.headers.is_some() || self.body.is_some()
    }
}

pub type MockStore = Arc<RwLock<HashMap<String, Vec<MockConfig>>>>;

/// Per-mock sequence call counters, keyed by "METHOD:/path#<index>"
pub type SequenceCounters = Arc<RwLock<HashMap<String, usize>>>;

// ============================================================================
// Request History Types
// ============================================================================

/// A recorded incoming request and its outcome
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: u64,
    pub timestamp: String,
    pub method: String,
    pub path: String,
    pub query_params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_mock: Option<String>,
    pub response_status: u16,
}

pub type RequestLog = Arc<tokio::sync::RwLock<Vec<RequestRecord>>>;

/// Create a lookup key from method and path (without query string)
pub fn create_mock_key(method: &str, path: &str) -> String {
    // Strip query string if present for the key
    let path_only = path.split('?').next().unwrap_or(path);
    format!("{}:{}", method.to_uppercase(), path_only)
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
        // Query string should be stripped from the key
        assert_eq!(
            create_mock_key("GET", "/users?page=1&limit=10"),
            "GET:/users"
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
            consume_body: true,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            sequence: None,
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
            consume_body: true,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            sequence: None,
        };

        let cloned = config.clone();
        assert_eq!(cloned.method, config.method);
        assert_eq!(cloned.path, config.path);
        assert_eq!(cloned.status, config.status);
    }

    #[tokio::test]
    async fn test_mock_store_operations() {
        let mut map = HashMap::new();
        map.insert(
            "GET:/test".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/test".to_string(),
                status: 200,
                response: serde_json::json!({}),
                consume_body: true,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let store: MockStore = Arc::new(RwLock::new(map));
        let mocks = store.read().await;
        assert_eq!(mocks.len(), 1);
        assert_eq!(mocks["GET:/test"].len(), 1);
        assert!(mocks.contains_key("GET:/test"));
        assert!(!mocks.contains_key("POST:/test"));
    }

    #[test]
    fn test_mock_config_with_consume_body_true() {
        let json = r#"{
            "method": "POST",
            "path": "/upload",
            "status": 200,
            "response": {"uploaded": true},
            "consume_body": true
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "POST");
        assert_eq!(config.path, "/upload");
        assert_eq!(config.status, 200);
        assert!(config.consume_body);
    }

    #[test]
    fn test_mock_config_with_consume_body_false() {
        let json = r#"{
            "method": "POST",
            "path": "/no-consume",
            "status": 200,
            "response": {"message": "ok"},
            "consume_body": false
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "POST");
        assert_eq!(config.path, "/no-consume");
        assert!(!config.consume_body);
    }

    #[test]
    fn test_mock_config_without_consume_body_defaults_to_false() {
        // Test that consume_body defaults to false when not specified in JSON
        let json = r#"{
            "method": "POST",
            "path": "/default",
            "status": 201,
            "response": {"created": true}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "POST");
        assert_eq!(config.path, "/default");
        assert_eq!(config.status, 201);
        assert!(!config.consume_body); // Should default to false (fast by default)
    }

    #[test]
    fn test_mock_config_serialization_with_consume_body() {
        let config = MockConfig {
            method: "POST".to_string(),
            path: "/test".to_string(),
            status: 200,
            response: serde_json::json!({"test": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            sequence: None,
        };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"consume_body\":false"));
    }

    #[test]
    fn test_mock_config_with_query_params() {
        let json = r#"{
            "method": "GET",
            "path": "/search",
            "status": 200,
            "response": {"results": []},
            "query_params": {
                "params": {
                    "q": "test",
                    "page": "1"
                },
                "strict": false
            }
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "GET");
        assert!(config.query_params.is_some());
        let qp = config.query_params.unwrap();
        assert_eq!(qp.params.len(), 2);
        assert!(!qp.strict);
    }

    #[test]
    fn test_mock_config_with_headers() {
        let json = r#"{
            "method": "GET",
            "path": "/protected",
            "status": 200,
            "response": {"data": "secret"},
            "headers": {
                "required": {
                    "authorization": "Bearer token123"
                },
                "forbidden": ["x-debug"],
                "strict": false
            }
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "GET");
        assert!(config.headers.is_some());
        let h = config.headers.unwrap();
        assert_eq!(h.required.len(), 1);
        assert_eq!(h.forbidden.len(), 1);
    }

    #[test]
    fn test_mock_config_with_json_body_matcher() {
        let json = r#"{
            "method": "POST",
            "path": "/login",
            "status": 200,
            "response": {"token": "abc"},
            "body": {
                "type": "json",
                "partial": {"username": "admin"}
            }
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.method, "POST");
        assert!(config.body.is_some());
    }

    #[test]
    fn test_mock_config_has_advanced_matchers() {
        let simple = MockConfig {
            method: "GET".to_string(),
            path: "/test".to_string(),
            status: 200,
            response: serde_json::json!({}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            sequence: None,
        };
        assert!(!simple.has_advanced_matchers());

        let with_query = MockConfig {
            method: "GET".to_string(),
            path: "/test".to_string(),
            status: 200,
            response: serde_json::json!({}),
            consume_body: false,
            query_params: Some(QueryParamMatcher::default()),
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            sequence: None,
        };
        assert!(with_query.has_advanced_matchers());
    }

    #[test]
    fn test_sequence_deserialization() {
        let json = r#"{
            "method": "POST",
            "path": "/api/submit",
            "status": 200,
            "response": {"ok": true},
            "sequence": [
                { "status": 200, "response": { "ok": true } },
                { "status": 429, "response": { "error": "rate limited" }, "delay_ms": 0 },
                { "status": 200, "response": { "ok": true }, "repeat": true }
            ]
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        let steps = config.sequence.unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].status, 200);
        assert!(!steps[0].repeat);
        assert_eq!(steps[1].status, 429);
        assert_eq!(steps[1].delay_ms, Some(0));
        assert_eq!(steps[1].response["error"], "rate limited");
        assert!(steps[2].repeat);
    }

    #[test]
    fn test_sequence_step_defaults() {
        let json = r#"{ "status": 503, "response": {"error": "unavailable"} }"#;
        let step: SequenceStep = serde_json::from_str(json).unwrap();
        assert_eq!(step.status, 503);
        assert_eq!(step.delay_ms, None);
        assert!(!step.repeat);
    }

    #[test]
    fn test_response_headers_deserialization() {
        let json = r#"{
            "method": "GET",
            "path": "/data.xml",
            "status": 200,
            "response_headers": {
                "Content-Type": "application/xml; charset=utf-8",
                "Cache-Control": "no-cache",
                "X-Custom-Header": "my-value"
            },
            "response": "<users><user id=\"1\"/></users>"
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        let headers = config.response_headers.unwrap();
        assert_eq!(headers.len(), 3);
        assert_eq!(
            headers.get("Content-Type").map(String::as_str),
            Some("application/xml; charset=utf-8")
        );
        assert_eq!(
            headers.get("X-Custom-Header").map(String::as_str),
            Some("my-value")
        );
    }

    #[test]
    fn test_mock_config_without_response_headers_backward_compat() {
        let json = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert!(config.response_headers.is_none());

        // Serialized output must omit the field so round-trips stay clean
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("response_headers"));
    }

    #[test]
    fn test_delay_config_fixed_deserialization() {
        let json = r#"{
            "method": "GET",
            "path": "/slow",
            "status": 200,
            "delay_ms": 500,
            "response": {"data": "finally here"}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config.delay_ms, Some(DelayConfig::Fixed(500))));
    }

    #[test]
    fn test_delay_config_range_deserialization() {
        let json = r#"{
            "method": "GET",
            "path": "/flaky",
            "status": 200,
            "delay_ms": { "min": 100, "max": 3000 },
            "response": {"data": "..."}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(
            config.delay_ms,
            Some(DelayConfig::Range {
                min: 100,
                max: 3000
            })
        ));
    }

    #[test]
    fn test_delay_config_resolve_fixed() {
        assert_eq!(DelayConfig::Fixed(250).resolve(), 250);
        assert_eq!(DelayConfig::Fixed(0).resolve(), 0);
    }

    #[test]
    fn test_delay_config_resolve_range_within_bounds() {
        let delay = DelayConfig::Range { min: 10, max: 20 };
        for _ in 0..100 {
            let ms = delay.resolve();
            assert!((10..=20).contains(&ms), "sampled {} outside 10..=20", ms);
        }
    }

    #[test]
    fn test_delay_config_resolve_range_degenerate() {
        // min == max resolves to that value
        assert_eq!(DelayConfig::Range { min: 50, max: 50 }.resolve(), 50);
        // Inverted range must not panic; resolves to min
        assert_eq!(DelayConfig::Range { min: 100, max: 10 }.resolve(), 100);
    }

    #[test]
    fn test_mock_config_without_delay_backward_compat() {
        let json = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert!(config.delay_ms.is_none());

        // Serialized output must omit the field so round-trips stay clean
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("delay_ms"));
    }

    #[test]
    fn test_mock_config_without_sequence_backward_compat() {
        let json = r#"{
            "method": "GET",
            "path": "/users",
            "status": 200,
            "response": {"users": []}
        }"#;

        let config: MockConfig = serde_json::from_str(json).unwrap();
        assert!(config.sequence.is_none());

        // Serialized output must omit the field so round-trips stay clean
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("sequence"));
    }
}
