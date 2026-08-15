use bytes::Bytes;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MockConfig {
    pub method: String,
    pub path: String,
    pub status: u16,

    /// The response body as JSON.
    ///
    /// Defaulted rather than required so a mock can serve its body from a
    /// [`MockConfig::response_file`] instead. With neither field present the
    /// value is `null`, which is what `"response": null` has always meant.
    #[serde(default)]
    pub response: serde_json::Value,

    /// A file whose bytes are the response body, resolved relative to the
    /// directory the mock file itself lives in.
    ///
    /// Mutually exclusive with a non-null `response`; the loader rejects a mock
    /// that sets both. See [`MockConfig::response_bytes`] for the bytes it
    /// resolves to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_file: Option<String>,

    /// Whether `{{...}}` templates inside a `response_file` body are rendered.
    ///
    /// Off by default, and ignored for binary content types: a PNG that happens
    /// to contain the bytes `{{` is a PNG, not a template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<bool>,

    /// The bytes of [`MockConfig::response_file`], read at load time.
    ///
    /// Loader-owned and skipped by serde in both directions: a mock file can't
    /// set it, and `/admin/mocks` won't dump a PDF into the dashboard. Reading
    /// at load time — and re-reading every hot-reload cycle — is what keeps
    /// request handling free of file I/O.
    #[serde(skip)]
    pub response_bytes: Option<Bytes>,

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

    /// Scenario tags this mock belongs to.
    ///
    /// Empty — the default, and the shape of every mock file written before
    /// this field existed — means "always matchable", so an untagged mock set
    /// behaves exactly as it did before scenarios were a thing. A mock with
    /// tags is matchable only while at least one of them is active; see
    /// [`MockConfig::is_active`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Path of the file this mock was loaded from, filled in by the loader so
    /// `/admin/mocks` can name the file to edit. `None` for mocks built
    /// in-process (tests, generated fixtures) rather than read from disk; a
    /// value present in the mock JSON itself is overwritten at load time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SequenceStep {
    pub status: u16,

    /// This step's body as JSON. Defaulted for the same reason
    /// [`MockConfig::response`] is: a step can serve a file instead.
    #[serde(default)]
    pub response: serde_json::Value,

    /// A file whose bytes are this step's body, resolved exactly like
    /// [`MockConfig::response_file`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_file: Option<String>,

    /// Whether templates inside this step's file body are rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<bool>,

    /// The bytes of this step's `response_file`, read at load time.
    #[serde(skip)]
    pub response_bytes: Option<Bytes>,

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

    /// Whether this mock's file body, or any of its steps', is templated *and*
    /// reads from the request body — the one thing that makes a file-backed
    /// mock need the request body it would otherwise skip reading.
    pub fn file_body_references_request_body(&self) -> bool {
        fn references(bytes: Option<&Bytes>, template: Option<bool>) -> bool {
            template.unwrap_or(false)
                && bytes.is_some_and(|b| {
                    std::str::from_utf8(b).is_ok_and(crate::template::text_references_body)
                })
        }

        references(self.response_bytes.as_ref(), self.template)
            || self
                .sequence
                .iter()
                .flatten()
                .any(|step| references(step.response_bytes.as_ref(), step.template))
    }

    /// Whether this mock can be matched under the given scenario selection.
    ///
    /// `active` is the set of currently active tags, or `None` when no
    /// scenario filter is configured at all. Two rules, both chosen so that
    /// adding the feature can't change what an existing mock set serves:
    ///
    /// - an untagged mock is always active, filter or no filter;
    /// - a tagged mock is active only when the filter is off, or when one of
    ///   its tags is in the active set (exact, case-sensitive match).
    pub fn is_active(&self, active: Option<&HashSet<String>>) -> bool {
        match active {
            None => true,
            Some(active_tags) => {
                self.tags.is_empty() || self.tags.iter().any(|tag| active_tags.contains(tag))
            }
        }
    }
}

pub type MockStore = Arc<RwLock<HashMap<String, Vec<MockConfig>>>>;

/// The scenario selection shared by the server: the set of active tags, or
/// `None` for "no filter, every mock is matchable" — the default, so a server
/// started without `MIMIC_ACTIVE_TAGS` behaves like one built before tags
/// existed. Wrapped in the same `RwLock` discipline as [`MockStore`] so
/// `POST /admin/scenario` can swap scenarios without racing in-flight matching.
pub type ActiveTags = Arc<RwLock<Option<HashSet<String>>>>;

/// Parse the comma-separated tag list from `MIMIC_ACTIVE_TAGS` (or a
/// `/admin/scenario` request) into a scenario selection.
///
/// Tags are trimmed and empties dropped, so `"a, b,"` is the two tags `a` and
/// `b`. A list that ends up with no tags at all means "no filter" (`None`),
/// which is what makes `MIMIC_ACTIVE_TAGS=` and an unset variable equivalent,
/// and lets `POST /admin/scenario {"tags": []}` clear the filter.
pub fn parse_active_tags<I, S>(tags: I) -> Option<HashSet<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let parsed: HashSet<String> = tags
        .into_iter()
        .flat_map(|tag| {
            tag.as_ref()
                .split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
        })
        .collect();

    (!parsed.is_empty()).then_some(parsed)
}

// ============================================================================
// Stable Mock Identity
// ============================================================================

/// What tells one mock apart from the others registered under the same
/// `METHOD:/path`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MockOrigin {
    /// The file the mock was loaded from. Stable for as long as the file is,
    /// which is exactly the lifetime the state keyed by it should have.
    Source(String),
    /// Position in the bucket, for a mock built in-process — tests, generated
    /// fixtures — with no file behind it. Carries the old semantics for the
    /// only case that can't do better.
    Position(usize),
}

/// A stable identity for one loaded mock.
///
/// State that has to outlive a hot reload — sequence positions, hit counts —
/// is keyed by this rather than by a mock's position in its bucket. The mock
/// map is rebuilt from scratch every reload cycle, so a position is a
/// statement about *this* cycle's vector: add a file that sorts earlier under
/// the same `METHOD:/path` and position 0 silently starts naming a different
/// mock, handing it a half-consumed sequence and another mock's hit count.
///
/// The file path is the stable half. A mock keeps its identity when a sibling
/// appears or disappears, and loses it — correctly — when its own file is
/// renamed or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MockIdentity {
    /// The registration key, `METHOD:/declared/path`. For a pattern mock this
    /// is the declared template (`GET:/users/:id`), not the concrete request
    /// path, so one sequence is shared across every id — as it always was.
    pub key: String,
    pub origin: MockOrigin,
}

impl MockIdentity {
    /// The identity of `mock`, registered under `key` at position `index`.
    pub fn of(mock: &MockConfig, key: &str, index: usize) -> Self {
        Self {
            key: key.to_string(),
            origin: match mock.source.as_deref() {
                Some(source) => MockOrigin::Source(source.to_string()),
                None => MockOrigin::Position(index),
            },
        }
    }

    /// The method half of the registration key.
    pub fn method(&self) -> &str {
        self.key.split_once(':').map_or("", |(method, _)| method)
    }

    /// The declared path half of the registration key.
    pub fn path(&self) -> &str {
        self.key
            .split_once(':')
            .map_or(self.key.as_str(), |(_, path)| path)
    }

    /// The file this mock came from, if it came from one.
    pub fn source(&self) -> Option<&str> {
        match &self.origin {
            MockOrigin::Source(source) => Some(source),
            MockOrigin::Position(_) => None,
        }
    }

    /// A human-readable rendering for the admin API, e.g.
    /// `POST:/orders @ mocks/orders_flow.json`.
    pub fn label(&self) -> String {
        match &self.origin {
            MockOrigin::Source(source) => format!("{} @ {}", self.key, source),
            MockOrigin::Position(index) => format!("{}#{}", self.key, index),
        }
    }
}

/// Per-mock sequence call counters, keyed by a mock's stable identity so a
/// hot reload can't hand one mock's position to another.
pub type SequenceCounters = Arc<RwLock<HashMap<MockIdentity, usize>>>;

// ============================================================================
// Request History Types
// ============================================================================

/// A recorded incoming request and its outcome.
///
/// Every field added after `response_status` is `#[serde(default)]`, so a
/// request log captured by an older Mimic still deserializes here and an
/// existing consumer of `/admin/requests` keeps seeing the shape it knows —
/// the new keys are simply absent when there's nothing to report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: u64,
    pub timestamp: String,
    pub method: String,
    pub path: String,
    pub query_params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_mock: Option<String>,
    pub response_status: u16,

    /// The response body actually served, truncated past
    /// [`crate::handler::max_recorded_body_size`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,

    /// The response headers actually served, with sensitive values redacted.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub response_headers: HashMap<String, String>,

    /// Score the winning mock earned, as computed by the matcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_score: Option<u32>,

    /// Values captured from the winning mock's path pattern (`/users/:id`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub path_params: HashMap<String, String>,

    /// Why this mock won, or — for an unmatched request — why each candidate
    /// was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_explanation: Option<String>,
}

pub type RequestLog = Arc<tokio::sync::RwLock<Vec<RequestRecord>>>;

/// True if `value` reads as an affirmative — in a query string
/// (`?unmatched_only=1`) or an environment variable (`MIMIC_CORS=true`).
///
/// An absent or empty value is false, so `?unmatched_only=` behaves like "off"
/// rather than failing the whole request.
pub fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

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
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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
                source: None,
                sequence: None,
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
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
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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

    // ========================================================================
    // RequestRecord: the admin API's wire shape
    // ========================================================================

    /// A record as an older Mimic would have written it: none of the fields
    /// added for the mock inspector.
    const LEGACY_RECORD: &str = r#"{
        "id": 7,
        "timestamp": "2026-01-01T00:00:00Z",
        "method": "GET",
        "path": "/users",
        "query_params": {"page": "1"},
        "headers": {"accept": "application/json"},
        "body": null,
        "matched_mock": "GET:/users",
        "response_status": 200
    }"#;

    #[test]
    fn request_record_reads_a_log_written_before_the_new_fields() {
        let record: RequestRecord = serde_json::from_str(LEGACY_RECORD).unwrap();

        assert_eq!(record.id, 7);
        assert_eq!(record.path, "/users");
        assert_eq!(record.matched_mock.as_deref(), Some("GET:/users"));
        // Everything new defaults to "nothing to report" rather than failing
        assert!(record.response_body.is_none());
        assert!(record.response_headers.is_empty());
        assert!(record.match_score.is_none());
        assert!(record.path_params.is_empty());
        assert!(record.match_explanation.is_none());
    }

    #[test]
    fn request_record_omits_empty_new_fields() {
        // An existing consumer of /admin/requests must not suddenly start
        // seeing a wall of nulls it has to skip past.
        let record = RequestRecord {
            id: 1,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            method: "GET".to_string(),
            path: "/users".to_string(),
            response_status: 200,
            ..Default::default()
        };

        let serialized = serde_json::to_string(&record).unwrap();
        for absent in [
            "response_body",
            "response_headers",
            "match_score",
            "path_params",
            "match_explanation",
        ] {
            assert!(
                !serialized.contains(absent),
                "{} should be omitted when empty: {}",
                absent,
                serialized
            );
        }
        // The fields that were always there stay unconditional
        assert!(serialized.contains("\"response_status\":200"));
    }

    #[test]
    fn request_record_serializes_the_new_fields_when_present() {
        let record = RequestRecord {
            id: 1,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            method: "GET".to_string(),
            path: "/users/42".to_string(),
            response_status: 200,
            response_body: Some(r#"{"id":42}"#.to_string()),
            response_headers: HashMap::from([(
                "content-type".to_string(),
                "application/json".to_string(),
            )]),
            match_score: Some(900),
            path_params: HashMap::from([("id".to_string(), "42".to_string())]),
            match_explanation: Some("matched mocks/get_user.json".to_string()),
            ..Default::default()
        };

        let round_tripped: RequestRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();

        assert_eq!(round_tripped.response_body, record.response_body);
        assert_eq!(round_tripped.match_score, Some(900));
        assert_eq!(
            round_tripped.path_params.get("id").map(String::as_str),
            Some("42")
        );
        assert_eq!(
            round_tripped.match_explanation.as_deref(),
            Some("matched mocks/get_user.json")
        );
    }

    #[test]
    fn mock_config_source_is_optional_and_omitted_when_unset() {
        let config: MockConfig =
            serde_json::from_str(r#"{"method":"GET","path":"/x","status":200,"response":{}}"#)
                .unwrap();
        assert!(config.source.is_none());
        assert!(!serde_json::to_string(&config).unwrap().contains("source"));
    }

    // ========================================================================
    // Scenario tags (#62)
    // ========================================================================

    fn tagged(tags: &[&str]) -> MockConfig {
        MockConfig {
            method: "GET".to_string(),
            path: "/x".to_string(),
            status: 200,
            response: serde_json::json!({}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            response_file: None,
            template: None,
            response_bytes: None,
        }
    }

    fn active(tags: &[&str]) -> HashSet<String> {
        tags.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn mock_config_without_tags_deserializes_to_an_empty_tag_list() {
        let config: MockConfig =
            serde_json::from_str(r#"{"method":"GET","path":"/x","status":200,"response":{}}"#)
                .unwrap();
        assert!(config.tags.is_empty());
        // Nothing new appears in the serialized form either, so a round trip
        // through Mimic doesn't rewrite an existing mock file's shape.
        assert!(!serde_json::to_string(&config).unwrap().contains("tags"));
    }

    #[test]
    fn mock_config_tags_round_trip() {
        let config: MockConfig = serde_json::from_str(
            r#"{"method":"POST","path":"/checkout","status":500,
                "response":{},"tags":["error-scenario","chaos"]}"#,
        )
        .unwrap();
        assert_eq!(config.tags, vec!["error-scenario", "chaos"]);
        assert!(serde_json::to_string(&config)
            .unwrap()
            .contains("error-scenario"));
    }

    #[test]
    fn untagged_mocks_are_active_with_and_without_a_filter() {
        let mock = tagged(&[]);
        assert!(mock.is_active(None));
        assert!(mock.is_active(Some(&active(&["happy-path"]))));
        assert!(mock.is_active(Some(&HashSet::new())));
    }

    #[test]
    fn tagged_mocks_need_one_active_tag() {
        let mock = tagged(&["error-scenario", "chaos"]);
        // No filter configured: tags are inert.
        assert!(mock.is_active(None));
        // One overlapping tag is enough.
        assert!(mock.is_active(Some(&active(&["chaos"]))));
        assert!(mock.is_active(Some(&active(&["chaos", "happy-path"]))));
        // Disjoint sets hide the mock.
        assert!(!mock.is_active(Some(&active(&["happy-path"]))));
        assert!(!mock.is_active(Some(&HashSet::new())));
    }

    #[test]
    fn tag_matching_is_case_sensitive() {
        let mock = tagged(&["Error-Scenario"]);
        assert!(!mock.is_active(Some(&active(&["error-scenario"]))));
        assert!(mock.is_active(Some(&active(&["Error-Scenario"]))));
    }

    #[test]
    fn parse_active_tags_splits_trims_and_drops_empties() {
        assert_eq!(
            parse_active_tags(Some("happy-path, smoke-test ,")),
            Some(active(&["happy-path", "smoke-test"]))
        );
        assert_eq!(
            parse_active_tags(vec!["a,b", "c"]),
            Some(active(&["a", "b", "c"]))
        );
    }

    #[test]
    fn parse_active_tags_treats_nothing_as_no_filter() {
        assert_eq!(parse_active_tags(None::<String>), None);
        assert_eq!(parse_active_tags(Some("")), None);
        assert_eq!(parse_active_tags(Some("   , ,")), None);
        assert_eq!(parse_active_tags(Vec::<String>::new()), None);
    }
}
