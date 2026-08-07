//! Request matching module for advanced mock matching.
//!
//! This module provides matching logic for:
//! - Query parameters
//! - HTTP headers
//! - Request body (JSON, text, form)

use crate::regex_cache::{cached, compiled_regex, CompileCache};
use crate::types::{
    is_sensitive_header, BodyMatcher, FormBodyMatcher, HeaderMatcher, HeaderPattern, HeaderValue,
    JsonBodyMatcher, MockConfig, QueryParamMatcher, QueryParamPattern, QueryParamValue,
    TextBodyMatcher,
};
use bytes::Bytes;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, OnceLock};
use tracing::{debug, warn};

// ============================================================================
// Rejection Reasons - why a matcher turned a mock down
// ============================================================================

/// Placeholder substituted for any value belonging to a sensitive header.
const REDACTED: &str = "[REDACTED]";

/// Longest value echoed back inside a rejection reason, in characters.
const MAX_DISPLAY_LEN: usize = 80;

/// The first thing about a request that made a mock's matchers turn it down.
///
/// Produced by the `*_reject_reason` functions — which *are* the matchers,
/// with the `match_*` booleans defined as `reason.is_none()` — so an
/// explanation can never describe a rule the server doesn't enforce.
///
/// Values carried here are display-ready: already truncated, and already
/// redacted when they belong to a sensitive header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    QueryParamMissing {
        name: String,
    },
    QueryParamMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    /// `strict: true` and the request carried a query param the mock doesn't declare
    QueryParamExtra {
        name: String,
    },
    HeaderMissing {
        name: String,
    },
    HeaderMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    HeaderForbidden {
        name: String,
    },
    /// `strict: true` and the request carried a header the mock doesn't declare
    HeaderExtra {
        name: String,
    },
    /// The mock declares a body matcher, but no body was read for this request
    BodyAbsent,
    /// The mock expects an empty body and the request carried one
    BodyNotEmpty,
    /// The body couldn't be read as the kind the matcher expects
    BodyWrongKind {
        expected: &'static str,
    },
    JsonExact,
    JsonFieldMissing {
        field: String,
    },
    JsonFieldMismatch {
        field: String,
    },
    JsonFieldExtra {
        field: String,
    },
    JsonArrayLength {
        field: String,
        expected: usize,
        actual: usize,
    },
    TextExact,
    TextContains {
        substring: String,
    },
    TextRegex {
        pattern: String,
    },
    /// A configured regex failed to compile, so nothing can match it
    InvalidRegex {
        pattern: String,
    },
    FormFieldMissing {
        field: String,
    },
    FormFieldMismatch {
        field: String,
    },
    FormFieldExtra {
        field: String,
    },
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::QueryParamMissing { name } => {
                write!(f, "required query param `{}` was absent", name)
            }
            RejectReason::QueryParamMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "query param `{}` was `{}`, expected {}",
                name, actual, expected
            ),
            RejectReason::QueryParamExtra { name } => write!(
                f,
                "strict query matching rejected the undeclared param `{}`",
                name
            ),
            RejectReason::HeaderMissing { name } => {
                write!(f, "required header `{}` was absent", name)
            }
            RejectReason::HeaderMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "header `{}` was `{}`, expected {}",
                name, actual, expected
            ),
            RejectReason::HeaderForbidden { name } => {
                write!(f, "forbidden header `{}` was present", name)
            }
            RejectReason::HeaderExtra { name } => write!(
                f,
                "strict header matching rejected the undeclared header `{}`",
                name
            ),
            RejectReason::BodyAbsent => {
                write!(f, "the mock requires a request body and none was sent")
            }
            RejectReason::BodyNotEmpty => {
                write!(f, "the mock requires an empty body and one was sent")
            }
            RejectReason::BodyWrongKind { expected } => {
                write!(f, "the body could not be read as {}", expected)
            }
            RejectReason::JsonExact => write!(f, "the JSON body did not match `exact`"),
            RejectReason::JsonFieldMissing { field } => {
                write!(f, "JSON body is missing the field `{}`", field)
            }
            RejectReason::JsonFieldMismatch { field } => {
                write!(f, "JSON body field `{}` has a different value", field)
            }
            RejectReason::JsonFieldExtra { field } => write!(
                f,
                "strict JSON matching rejected the undeclared field `{}`",
                field
            ),
            RejectReason::JsonArrayLength {
                field,
                expected,
                actual,
            } => write!(
                f,
                "strict JSON matching expected {} item(s) in `{}`, got {}",
                expected, field, actual
            ),
            RejectReason::TextExact => write!(f, "the text body did not match `exact`"),
            RejectReason::TextContains { substring } => {
                write!(f, "the text body does not contain `{}`", substring)
            }
            RejectReason::TextRegex { pattern } => {
                write!(f, "the text body does not match the regex `{}`", pattern)
            }
            RejectReason::InvalidRegex { pattern } => {
                write!(f, "the configured regex `{}` is invalid", pattern)
            }
            RejectReason::FormFieldMissing { field } => {
                write!(f, "form body is missing the field `{}`", field)
            }
            RejectReason::FormFieldMismatch { field } => {
                write!(f, "form body field `{}` has a different value", field)
            }
            RejectReason::FormFieldExtra { field } => write!(
                f,
                "strict form matching rejected the undeclared field `{}`",
                field
            ),
        }
    }
}

/// Shorten `value` to [`MAX_DISPLAY_LEN`] characters (not bytes, so a
/// multi-byte character is never cut in half), marking anything dropped.
fn truncate_for_display(value: &str) -> String {
    match value.char_indices().nth(MAX_DISPLAY_LEN) {
        Some((byte_idx, _)) => format!("{}…", &value[..byte_idx]),
        None => value.to_string(),
    }
}

/// Describe the root of a dotted field path, which is empty when the mismatch
/// is the JSON document itself rather than a field within it.
fn describe_field_path(field_path: &str) -> String {
    if field_path.is_empty() {
        "(root)".to_string()
    } else {
        field_path.to_string()
    }
}

/// Render a query param expectation the way a human would read it.
fn describe_query_value(expected: &QueryParamValue) -> String {
    match expected {
        QueryParamValue::Exact(value) => format!("`{}`", truncate_for_display(value)),
        QueryParamValue::Pattern(QueryParamPattern::Regex(pattern)) => {
            format!("a match for `{}`", truncate_for_display(pattern))
        }
        QueryParamValue::Pattern(QueryParamPattern::Any) => "any value".to_string(),
    }
}

/// Render a header expectation the way a human would read it.
fn describe_header_value(expected: &HeaderValue) -> String {
    match expected {
        HeaderValue::Exact(value) => format!("`{}`", truncate_for_display(value)),
        HeaderValue::Pattern(HeaderPattern::Regex(pattern)) => {
            format!("a match for `{}`", truncate_for_display(pattern))
        }
        HeaderValue::Pattern(HeaderPattern::Any) => "any value".to_string(),
        HeaderValue::Pattern(HeaderPattern::Prefix(prefix)) => {
            format!("a value starting with `{}`", truncate_for_display(prefix))
        }
        HeaderValue::Pattern(HeaderPattern::Contains(substring)) => {
            format!("a value containing `{}`", truncate_for_display(substring))
        }
    }
}

/// A map's entries, ordered by key.
///
/// `HashMap` iteration order is reseeded per process, so walking a matcher's
/// requirements directly would let two equally-failing rules take turns being
/// "the reason" across restarts. Explanations are compared and screenshotted
/// by humans; they need to be reproducible.
fn sorted_by_key<V>(map: &HashMap<String, V>) -> Vec<(&String, &V)> {
    let mut entries: Vec<(&String, &V)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// A map's keys, ordered — see [`sorted_by_key`].
fn sorted_keys<V>(map: &HashMap<String, V>) -> Vec<&String> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys
}

// ============================================================================
// Request Context - Parsed request data for matching
// ============================================================================

/// Parsed request data used for matching against mocks
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub method: String,
    pub path: String,
    /// Named path parameters (e.g. `:id`). Always empty until route-level
    /// path parameter matching is implemented; reserved so templating
    /// (`{{path.id}}`) has a source to read from once it lands.
    pub path_params: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
    pub content_type: Option<String>,
}

impl RequestContext {
    #[allow(dead_code)]
    pub fn new(method: String, path: String) -> Self {
        Self {
            method,
            path,
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            content_type: None,
        }
    }
}

// ============================================================================
// Query Parameter Parsing and Matching
// ============================================================================

/// Parse query string into HashMap
/// Example: "page=1&limit=10" -> {"page": "1", "limit": "10"}
pub fn parse_query_string(query: Option<&str>) -> HashMap<String, String> {
    let mut params = HashMap::new();

    if let Some(q) = query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }

            if let Some((key, value)) = pair.split_once('=') {
                // URL decode key and value
                let key = urlencoding::decode(key).unwrap_or_default().to_string();
                let value = urlencoding::decode(value).unwrap_or_default().to_string();
                params.insert(key, value);
            } else {
                // Handle params without values (e.g., ?debug)
                let key = urlencoding::decode(pair).unwrap_or_default().to_string();
                params.insert(key, String::new());
            }
        }
    }

    params
}

/// Check if request query params match the mock's requirements
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
pub fn match_query_params(
    request_params: &HashMap<String, String>,
    matcher: &QueryParamMatcher,
) -> bool {
    query_params_reject_reason(request_params, matcher).is_none()
}

/// Why `matcher` rejects `request_params`, or `None` if it accepts them.
///
/// This is the query-parameter matcher; [`match_query_params`] is the boolean
/// view of it. Keeping one implementation is what stops the dashboard's
/// explanations from describing a matcher the server doesn't actually run.
pub fn query_params_reject_reason(
    request_params: &HashMap<String, String>,
    matcher: &QueryParamMatcher,
) -> Option<RejectReason> {
    // Check all required params match. Iterated in name order so a mock with
    // several failing params always reports the same one.
    for (key, expected_value) in sorted_by_key(&matcher.params) {
        match request_params.get(key) {
            Some(actual_value) => {
                if !match_query_param_value(actual_value, expected_value) {
                    debug!(
                        "Query param '{}' mismatch: expected {:?}, got '{}'",
                        key, expected_value, actual_value
                    );
                    return Some(RejectReason::QueryParamMismatch {
                        name: key.clone(),
                        expected: describe_query_value(expected_value),
                        actual: truncate_for_display(actual_value),
                    });
                }
            }
            None => {
                debug!("Required query param '{}' is missing", key);
                return Some(RejectReason::QueryParamMissing { name: key.clone() });
            }
        }
    }

    // If strict mode, check for extra params
    if matcher.strict {
        for key in sorted_keys(request_params) {
            if !matcher.params.contains_key(key) {
                debug!("Strict mode: extra query param '{}' found", key);
                return Some(RejectReason::QueryParamExtra { name: key.clone() });
            }
        }
    }

    None
}

/// Match a single query parameter value
fn match_query_param_value(actual: &str, expected: &QueryParamValue) -> bool {
    match expected {
        QueryParamValue::Exact(value) => actual == value,
        QueryParamValue::Pattern(pattern) => match pattern {
            QueryParamPattern::Regex(regex_str) => match compiled_regex(regex_str) {
                Some(re) => re.is_match(actual),
                None => false,
            },
            QueryParamPattern::Any => true,
        },
    }
}

// ============================================================================
// Header Parsing and Matching
// ============================================================================

/// Parse HTTP headers into HashMap (normalized to lowercase)
pub fn parse_headers(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    let mut parsed = HashMap::new();

    for (name, value) in headers.iter() {
        let name_str = name.as_str().to_lowercase();

        if let Ok(value_str) = value.to_str() {
            parsed.insert(name_str, value_str.to_string());
        }
    }

    parsed
}

/// Headers that `strict: true` never counts as "extra".
///
/// These are sent unconditionally by mainstream HTTP clients (curl, browsers,
/// `fetch`, Postman, most libraries), so counting them would make strict mode
/// reject essentially all real traffic unless every mock declared them.
/// `accept` in particular is sent by curl (`Accept: */*`) and by every browser
/// on every request.
pub const IGNORED_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "connection",
    "content-length",
    "host",
    "user-agent",
];

/// Check if request headers match the mock's requirements
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
pub fn match_headers(request_headers: &HashMap<String, String>, matcher: &HeaderMatcher) -> bool {
    headers_reject_reason(request_headers, matcher).is_none()
}

/// Why `matcher` rejects `request_headers`, or `None` if it accepts them.
///
/// This is the header matcher; [`match_headers`] is the boolean view of it.
///
/// Header *values* — both the request's and the mock's expectation — are
/// redacted here, at construction, whenever the header is sensitive. The
/// reason value therefore never carries a credential in the first place, so
/// no downstream formatting or serialization can leak one.
pub fn headers_reject_reason(
    request_headers: &HashMap<String, String>,
    matcher: &HeaderMatcher,
) -> Option<RejectReason> {
    // Check all required headers match, in name order for a stable report
    for (name, expected_value) in sorted_by_key(&matcher.required) {
        let normalized_name = name.to_lowercase();

        match request_headers.get(&normalized_name) {
            Some(actual_value) => {
                if !match_header_value(actual_value, expected_value) {
                    debug!(
                        "Header '{}' mismatch: expected {:?}, got '{}'",
                        name, expected_value, actual_value
                    );
                    let sensitive = is_sensitive_header(&normalized_name);
                    return Some(RejectReason::HeaderMismatch {
                        name: normalized_name,
                        // Backticked to match the shape of a real expectation,
                        // so both halves of the message read alike
                        expected: if sensitive {
                            format!("`{}`", REDACTED)
                        } else {
                            describe_header_value(expected_value)
                        },
                        actual: if sensitive {
                            REDACTED.to_string()
                        } else {
                            truncate_for_display(actual_value)
                        },
                    });
                }
            }
            None => {
                debug!("Required header '{}' is missing", name);
                return Some(RejectReason::HeaderMissing {
                    name: normalized_name,
                });
            }
        }
    }

    // Check forbidden headers are not present
    for forbidden_name in &matcher.forbidden {
        let normalized_name = forbidden_name.to_lowercase();

        if request_headers.contains_key(&normalized_name) {
            debug!("Forbidden header '{}' is present", forbidden_name);
            return Some(RejectReason::HeaderForbidden {
                name: normalized_name,
            });
        }
    }

    // If strict mode, check for extra headers (excluding standard ones)
    if matcher.strict {
        for name in sorted_keys(request_headers) {
            if IGNORED_HEADERS.contains(&name.as_str()) {
                continue;
            }

            let is_required = matcher
                .required
                .keys()
                .any(|req| req.to_lowercase() == *name);

            if !is_required {
                debug!("Strict mode: extra header '{}' found", name);
                return Some(RejectReason::HeaderExtra { name: name.clone() });
            }
        }
    }

    None
}

/// Match a single header value
fn match_header_value(actual: &str, expected: &HeaderValue) -> bool {
    match expected {
        HeaderValue::Exact(value) => actual == value,
        HeaderValue::Pattern(pattern) => match pattern {
            HeaderPattern::Regex(regex_str) => match compiled_regex(regex_str) {
                Some(re) => re.is_match(actual),
                None => false,
            },
            HeaderPattern::Any => true,
            HeaderPattern::Prefix(prefix) => actual.starts_with(prefix),
            HeaderPattern::Contains(substring) => actual.contains(substring),
        },
    }
}

// ============================================================================
// Body Parsing and Matching
// ============================================================================

/// Parsed body content
#[derive(Debug, Clone)]
pub enum ParsedBody {
    Json(serde_json::Value),
    Text(String),
    Form(HashMap<String, String>),
    #[allow(dead_code)]
    Binary(Vec<u8>),
    Empty,
}

/// Parse request body based on content type
pub fn parse_body(bytes: &Bytes, content_type: Option<&str>) -> ParsedBody {
    if bytes.is_empty() {
        return ParsedBody::Empty;
    }

    match content_type {
        Some(ct) if ct.contains("application/json") => {
            match serde_json::from_slice(bytes) {
                Ok(json) => ParsedBody::Json(json),
                Err(e) => {
                    warn!("Failed to parse JSON body: {}", e);
                    // Fall back to text
                    ParsedBody::Text(String::from_utf8_lossy(bytes).to_string())
                }
            }
        }

        Some(ct) if ct.contains("application/x-www-form-urlencoded") => {
            let text = String::from_utf8_lossy(bytes);
            let form = parse_form_data(&text);
            ParsedBody::Form(form)
        }

        Some(ct) if ct.starts_with("text/") => {
            ParsedBody::Text(String::from_utf8_lossy(bytes).to_string())
        }

        _ => {
            // Try to parse as text, fall back to binary
            match String::from_utf8(bytes.to_vec()) {
                Ok(text) => ParsedBody::Text(text),
                Err(_) => ParsedBody::Binary(bytes.to_vec()),
            }
        }
    }
}

/// Parse form data (application/x-www-form-urlencoded)
fn parse_form_data(text: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();

    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }

        if let Some((key, value)) = pair.split_once('=') {
            let key = urlencoding::decode(key).unwrap_or_default().to_string();
            let value = urlencoding::decode(value).unwrap_or_default().to_string();
            fields.insert(key, value);
        }
    }

    fields
}

/// Check if request body matches the mock's requirements
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
pub fn match_body(parsed_body: &ParsedBody, matcher: &BodyMatcher) -> bool {
    body_reject_reason(parsed_body, matcher).is_none()
}

/// Why `matcher` rejects `parsed_body`, or `None` if it accepts it.
///
/// This is the body matcher; [`match_body`] is the boolean view of it.
///
/// Reasons name the *shape* of the mismatch — which JSON field, which form
/// field, which configured substring — and never quote the request body back.
/// The body is already stored on the record verbatim; repeating it inside an
/// explanation only widens the surface a secret can appear on.
pub fn body_reject_reason(parsed_body: &ParsedBody, matcher: &BodyMatcher) -> Option<RejectReason> {
    match matcher {
        BodyMatcher::Any => None,

        BodyMatcher::Empty => {
            if matches!(parsed_body, ParsedBody::Empty) {
                None
            } else {
                Some(RejectReason::BodyNotEmpty)
            }
        }

        BodyMatcher::Json(json_matcher) => {
            if let ParsedBody::Json(actual) = parsed_body {
                json_body_reject_reason(actual, json_matcher)
            } else if let ParsedBody::Text(text) = parsed_body {
                // Try to parse text as JSON
                if let Ok(actual) = serde_json::from_str(text) {
                    json_body_reject_reason(&actual, json_matcher)
                } else {
                    debug!("Body is not valid JSON");
                    Some(RejectReason::BodyWrongKind { expected: "JSON" })
                }
            } else {
                debug!("Expected JSON body, got {:?}", parsed_body);
                Some(RejectReason::BodyWrongKind { expected: "JSON" })
            }
        }

        BodyMatcher::Text(text_matcher) => {
            if let ParsedBody::Text(actual) = parsed_body {
                text_body_reject_reason(actual, text_matcher)
            } else if let ParsedBody::Json(json) = parsed_body {
                // Convert JSON to string for text matching
                let text = json.to_string();
                text_body_reject_reason(&text, text_matcher)
            } else {
                debug!("Expected text body");
                Some(RejectReason::BodyWrongKind { expected: "text" })
            }
        }

        BodyMatcher::Form(form_matcher) => {
            if let ParsedBody::Form(actual) = parsed_body {
                form_body_reject_reason(actual, form_matcher)
            } else if let ParsedBody::Text(text) = parsed_body {
                // Try to parse as form data
                let form = parse_form_data(text);
                form_body_reject_reason(&form, form_matcher)
            } else {
                debug!("Expected form body");
                Some(RejectReason::BodyWrongKind {
                    expected: "form data",
                })
            }
        }
    }
}

#[allow(dead_code)] // boolean view of the matcher below; exercised by the test suite
fn match_json_body(actual: &serde_json::Value, matcher: &JsonBodyMatcher) -> bool {
    json_body_reject_reason(actual, matcher).is_none()
}

/// Why a JSON body matcher rejects `actual`, or `None` if it accepts it.
fn json_body_reject_reason(
    actual: &serde_json::Value,
    matcher: &JsonBodyMatcher,
) -> Option<RejectReason> {
    // Exact match
    if let Some(ref expected) = matcher.exact {
        if actual != expected {
            debug!("JSON exact match failed");
            return Some(RejectReason::JsonExact);
        }
    }

    // Partial match
    if let Some(ref expected) = matcher.partial {
        if let Some(reason) = json_partial_reject_reason(actual, expected, matcher.strict, "") {
            debug!("JSON partial match failed");
            return Some(reason);
        }
    }

    None
}

/// Partial JSON match: check if actual contains all fields from expected
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
fn match_json_partial(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    strict: bool,
) -> bool {
    json_partial_reject_reason(actual, expected, strict, "").is_none()
}

/// Why a partial JSON match rejects `actual`, or `None` if it accepts it.
///
/// `field_path` is the dotted path walked so far (empty at the root), so a
/// nested mismatch can name `user.address.city` rather than just "some field".
fn json_partial_reject_reason(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    strict: bool,
    field_path: &str,
) -> Option<RejectReason> {
    /// Extend a dotted field path with one more segment.
    fn child(field_path: &str, key: &str) -> String {
        if field_path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{}", field_path, key)
        }
    }

    match (actual, expected) {
        (serde_json::Value::Object(actual_obj), serde_json::Value::Object(expected_obj)) => {
            // Check all expected fields exist and match
            for (key, expected_value) in expected_obj {
                match actual_obj.get(key) {
                    Some(actual_value) => {
                        if let Some(reason) = json_partial_reject_reason(
                            actual_value,
                            expected_value,
                            strict,
                            &child(field_path, key),
                        ) {
                            return Some(reason);
                        }
                    }
                    None => {
                        debug!("Expected field '{}' missing in actual", key);
                        return Some(RejectReason::JsonFieldMissing {
                            field: child(field_path, key),
                        });
                    }
                }
            }

            // If strict, check for extra fields
            if strict {
                for key in actual_obj.keys() {
                    if !expected_obj.contains_key(key) {
                        debug!("Strict mode: extra field '{}' in actual", key);
                        return Some(RejectReason::JsonFieldExtra {
                            field: child(field_path, key),
                        });
                    }
                }
            }

            None
        }

        (serde_json::Value::Array(actual_arr), serde_json::Value::Array(expected_arr)) => {
            if strict && actual_arr.len() != expected_arr.len() {
                return Some(RejectReason::JsonArrayLength {
                    field: describe_field_path(field_path),
                    expected: expected_arr.len(),
                    actual: actual_arr.len(),
                });
            }

            // Check if all expected items match in order
            for (i, expected_item) in expected_arr.iter().enumerate() {
                if let Some(actual_item) = actual_arr.get(i) {
                    if let Some(reason) = json_partial_reject_reason(
                        actual_item,
                        expected_item,
                        strict,
                        &format!("{}[{}]", field_path, i),
                    ) {
                        return Some(reason);
                    }
                } else {
                    return Some(RejectReason::JsonFieldMissing {
                        field: format!("{}[{}]", field_path, i),
                    });
                }
            }

            None
        }

        _ => {
            if actual == expected {
                None
            } else {
                Some(RejectReason::JsonFieldMismatch {
                    field: describe_field_path(field_path),
                })
            }
        }
    }
}

/// Match text body
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
fn match_text_body(actual: &str, matcher: &TextBodyMatcher) -> bool {
    text_body_reject_reason(actual, matcher).is_none()
}

/// Why a text body matcher rejects `actual`, or `None` if it accepts it.
fn text_body_reject_reason(actual: &str, matcher: &TextBodyMatcher) -> Option<RejectReason> {
    // Exact match
    if let Some(ref expected) = matcher.exact {
        if actual != expected {
            debug!("Text exact match failed");
            return Some(RejectReason::TextExact);
        }
    }

    // Contains
    if let Some(ref substring) = matcher.contains {
        if !actual.contains(substring) {
            debug!("Text does not contain '{}'", substring);
            return Some(RejectReason::TextContains {
                substring: truncate_for_display(substring),
            });
        }
    }

    // Regex
    if let Some(ref pattern) = matcher.regex {
        match compiled_regex(pattern) {
            Some(re) => {
                if !re.is_match(actual) {
                    debug!("Text does not match regex '{}'", pattern);
                    return Some(RejectReason::TextRegex {
                        pattern: truncate_for_display(pattern),
                    });
                }
            }
            None => {
                return Some(RejectReason::InvalidRegex {
                    pattern: truncate_for_display(pattern),
                })
            }
        }
    }

    None
}

/// Match form body
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
fn match_form_body(actual: &HashMap<String, String>, matcher: &FormBodyMatcher) -> bool {
    form_body_reject_reason(actual, matcher).is_none()
}

/// Why a form body matcher rejects `actual`, or `None` if it accepts it.
fn form_body_reject_reason(
    actual: &HashMap<String, String>,
    matcher: &FormBodyMatcher,
) -> Option<RejectReason> {
    // Check all required fields match, in name order for a stable report
    for (key, expected_value) in sorted_by_key(&matcher.fields) {
        match actual.get(key) {
            Some(actual_value) => {
                if actual_value != expected_value {
                    debug!(
                        "Form field '{}' mismatch: expected '{}', got '{}'",
                        key, expected_value, actual_value
                    );
                    return Some(RejectReason::FormFieldMismatch { field: key.clone() });
                }
            }
            None => {
                debug!("Required form field '{}' is missing", key);
                return Some(RejectReason::FormFieldMissing { field: key.clone() });
            }
        }
    }

    // If strict, check for extra fields
    if matcher.strict {
        for key in sorted_keys(actual) {
            if !matcher.fields.contains_key(key) {
                debug!("Strict mode: extra form field '{}' found", key);
                return Some(RejectReason::FormFieldExtra { field: key.clone() });
            }
        }
    }

    None
}

// ============================================================================
// Path Parameter Matching
// ============================================================================

/// A path template compiled into a regex, plus the parameter names in the
/// order their capture groups appear. Supports `:name` (Express-style) and
/// `{name}` (OpenAPI-style) segments.
#[derive(Debug, Clone)]
pub struct CompiledPathPattern {
    pub regex: Regex,
    pub param_names: Vec<String>,
}

/// True if any path segment uses `:name` or `{name}` parameter syntax.
pub fn is_pattern_path(path: &str) -> bool {
    path.split('/').any(|segment| {
        (segment.starts_with(':') && segment.len() > 1)
            || (segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2)
    })
}

/// Compile (or reuse) the pattern for `path`.
///
/// Path templates come from mock files, so the same handful of strings are
/// matched against on every request. Compiling them is memoized process-wide:
/// a given template costs one `Regex::new` for the lifetime of the process,
/// not one per request. Hot reload swapping the mock map doesn't invalidate
/// anything — the cache is keyed by the template string itself.
pub fn compile_path_pattern(path: &str) -> Option<Arc<CompiledPathPattern>> {
    cached(&PATH_PATTERN_CACHE, path, build_path_pattern)
}

static PATH_PATTERN_CACHE: CompileCache<CompiledPathPattern> = OnceLock::new();

/// True if `path` has ever been compiled in this process. Test-only probe for
/// asserting that a request never reached the path-pattern compiler.
#[cfg(test)]
pub fn path_pattern_is_cached(path: &str) -> bool {
    crate::regex_cache::is_cached(&PATH_PATTERN_CACHE, path)
}

/// Compile a path template into a regex with one (unnamed, positional)
/// capture group per parameter segment — using positional groups rather
/// than Rust regex's named-group syntax means parameter names aren't
/// restricted to `[A-Za-z0-9_]` (e.g. `:org-id` works).
///
/// Returns `None` if the path has no parameter segments, or the resulting
/// regex fails to compile.
fn build_path_pattern(path: &str) -> Option<CompiledPathPattern> {
    let mut param_names = Vec::new();
    let mut regex_parts = Vec::new();

    for segment in path.split('/') {
        if let Some(name) = segment.strip_prefix(':').filter(|n| !n.is_empty()) {
            param_names.push(name.to_string());
            regex_parts.push("([^/]+)".to_string());
        } else if segment.len() > 2 && segment.starts_with('{') && segment.ends_with('}') {
            param_names.push(segment[1..segment.len() - 1].to_string());
            regex_parts.push("([^/]+)".to_string());
        } else {
            regex_parts.push(regex::escape(segment));
        }
    }

    if param_names.is_empty() {
        return None;
    }

    let pattern = format!("^{}$", regex_parts.join("/"));
    match Regex::new(&pattern) {
        Ok(regex) => Some(CompiledPathPattern { regex, param_names }),
        Err(e) => {
            warn!("Invalid path parameter pattern '{}': {}", path, e);
            None
        }
    }
}

/// Match a concrete request path against a compiled pattern, returning the
/// captured parameter values keyed by name, or `None` if it doesn't match.
pub fn match_path_pattern(
    pattern: &CompiledPathPattern,
    request_path: &str,
) -> Option<HashMap<String, String>> {
    let caps = pattern.regex.captures(request_path)?;
    let mut params = HashMap::with_capacity(pattern.param_names.len());
    for (i, name) in pattern.param_names.iter().enumerate() {
        let value = caps.get(i + 1)?.as_str().to_string();
        params.insert(name.clone(), value);
    }
    Some(params)
}

// ============================================================================
// Mock Finding - Find best matching mock
// ============================================================================

/// Score deducted from a pattern (path-parameter) match relative to an exact
/// path match, so an exact path always outranks a pattern when both match.
const PATTERN_MATCH_PENALTY: u32 = 100;

/// Result of mock matching with score for ranking
#[derive(Debug)]
pub struct MatchResult {
    pub mock: MockConfig,
    pub score: u32,
    /// Position of the mock within its METHOD:PATH bucket, used to key
    /// per-mock sequence counters when several mocks share a path
    pub index: usize,
    /// Values captured from named path parameters; empty for exact matches
    pub path_params: HashMap<String, String>,
    /// The "METHOD:path" key the matched mock is registered under — the
    /// mock's declared path (which may be a pattern like `/users/:id`),
    /// not necessarily the concrete request path
    pub matched_key: String,
    /// Number of literal (non-parameter) segments in the mock's declared
    /// path, used to break score ties in favor of the more specific route
    pub specificity: u32,
    /// The components `score` was assembled from, so the win can be explained
    /// without re-running the matchers
    pub breakdown: ScoreBreakdown,
}

/// Count the literal — i.e. non-`:name`, non-`{name}` — segments of a declared
/// mock path. `/users/:id` scores 1, `/{resource}/:id` scores 0, and an exact
/// path scores one per segment.
fn path_specificity(path: &str) -> u32 {
    path.split('/')
        .filter(|segment| {
            let is_param = (segment.starts_with(':') && segment.len() > 1)
                || (segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2);
            !is_param && !segment.is_empty()
        })
        .count() as u32
}

/// Decide whether the request body must be read, knowing only the method and
/// path — this runs *before* the body is consumed, so the eventual matched
/// mock isn't known yet.
///
/// The body is read when either:
///
/// - some mock registered for this method+path asks for it, i.e. it declares a
///   `body` matcher, sets `consume_body: true`, or interpolates `{{body.…}}`
///   into its response; or
/// - no mock is registered for this method+path at all, so the request is
///   heading for a 404 and the body is worth capturing for the request log.
///
/// Otherwise the body is left unread, which is what `consume_body: false`
/// (the default) promises.
///
/// `active_tags` is the scenario selection (see [`MockConfig::is_active`]);
/// mocks filtered out by it are invisible here for the same reason they're
/// invisible to [`find_matching_mock`] — a request only tag-filtered mocks
/// could serve is heading for a 404, and its body is worth logging.
pub fn requires_body(
    method: &str,
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> bool {
    let mut any_candidate = false;

    /// A single mock's own answer to "do I need the request body?"
    fn mock_needs_body(mock: &MockConfig) -> bool {
        mock.consume_body
            || mock.body.is_some()
            || crate::template::references_body(&mock.response)
            || mock.sequence.as_deref().is_some_and(|steps| {
                steps
                    .iter()
                    .any(|step| crate::template::references_body(&step.response))
            })
    }

    let base_key = crate::types::create_mock_key(method, path);
    if let Some(mock_list) = mocks.get(&base_key) {
        let mut active = mock_list
            .iter()
            .filter(|m| m.is_active(active_tags))
            .peekable();
        if active.peek().is_some() {
            any_candidate = true;
            if active.any(mock_needs_body) {
                return true;
            }
        }
    }

    // Pattern routes can serve this path too, either as the winner or as the
    // fallback when the exact key's matchers reject the request.
    let method_prefix = format!("{}:", method.to_uppercase());
    for (key, mock_list) in mocks.iter() {
        if *key == base_key {
            continue;
        }
        let Some(path_part) = key.strip_prefix(method_prefix.as_str()) else {
            continue;
        };
        if !is_pattern_path(path_part) {
            continue;
        }
        let Some(pattern) = compile_path_pattern(path_part) else {
            continue;
        };
        if match_path_pattern(&pattern, path).is_none() {
            continue;
        }

        let mut active = mock_list
            .iter()
            .filter(|m| m.is_active(active_tags))
            .peekable();
        if active.peek().is_none() {
            continue;
        }
        any_candidate = true;
        if active.any(mock_needs_body) {
            return true;
        }
    }

    !any_candidate
}

/// Find the best matching mock for a request.
///
/// Exact "METHOD:path" lookups are O(1) and tried first. Parameterized paths
/// (`:id`, `{id}`) are only scanned when the exact lookup produced no match at
/// all, so a mock set with no path parameters — or a request that hits one
/// exactly — never touches the pattern loop. A pattern match also always
/// scores below an otherwise-equal exact match.
///
/// `active_tags` is the scenario selection: mocks it filters out (see
/// [`MockConfig::is_active`]) are skipped before scoring, so they behave
/// exactly as if they weren't loaded. `None`, and any mock without `tags`,
/// means no filtering — the zero-config behavior.
pub fn find_matching_mock(
    context: &RequestContext,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> Option<MatchResult> {
    let base_key = crate::types::create_mock_key(&context.method, &context.path);

    debug!(
        "Looking for mock: {} (key: {})",
        format!("{} {}", context.method, context.path),
        base_key
    );

    let mut candidates: Vec<MatchResult> = Vec::new();

    // Exact match: O(1) lookup, no path params, full score.
    if let Some(mock_list) = mocks.get(&base_key) {
        for (index, mock) in mock_list.iter().enumerate() {
            // Note the index comes from the unfiltered list: it keys sequence
            // counters and hit counts, so it must not shift when a scenario
            // hides a sibling mock registered under the same path.
            if !mock.is_active(active_tags) {
                continue;
            }
            if let Some(breakdown) = calculate_match_score(context, mock) {
                candidates.push(MatchResult {
                    mock: mock.clone(),
                    score: breakdown.total(),
                    index,
                    path_params: HashMap::new(),
                    matched_key: base_key.clone(),
                    specificity: path_specificity(&context.path),
                    breakdown,
                });
            }
        }
    }

    // Pattern match: only reached when nothing matched the exact key. Scans
    // mocks registered under the same method whose path uses :name/{name}
    // segments; each template's regex is compiled once process-wide, not
    // once per request.
    if candidates.is_empty() {
        let method_prefix = format!("{}:", context.method.to_uppercase());
        for (key, mock_list) in mocks.iter() {
            if *key == base_key {
                continue; // already handled as an exact match above
            }
            let Some(path_part) = key.strip_prefix(method_prefix.as_str()) else {
                continue;
            };
            if !is_pattern_path(path_part) {
                continue;
            }
            let Some(pattern) = compile_path_pattern(path_part) else {
                continue;
            };
            let Some(params) = match_path_pattern(&pattern, &context.path) else {
                continue;
            };

            for (index, mock) in mock_list.iter().enumerate() {
                if !mock.is_active(active_tags) {
                    continue;
                }
                if let Some(breakdown) = calculate_match_score(context, mock) {
                    candidates.push(MatchResult {
                        mock: mock.clone(),
                        score: breakdown.total().saturating_sub(PATTERN_MATCH_PENALTY),
                        index,
                        path_params: params.clone(),
                        matched_key: key.clone(),
                        specificity: path_specificity(path_part),
                        breakdown,
                    });
                }
            }
        }
    }

    // Rank candidates. Score decides first; the remaining keys exist so the
    // winner is a pure function of the mock set, never of `HashMap` iteration
    // order — which is reseeded on every hot reload and would otherwise let
    // two equally-scored pattern mocks trade places every couple of seconds.
    //
    //   1. highest score
    //   2. most literal path segments (`/users/:id` beats `/{any}/:id`)
    //   3. lowest registered key, lexicographically
    //   4. lowest position within that key's bucket
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.specificity.cmp(&a.specificity))
            .then_with(|| a.matched_key.cmp(&b.matched_key))
            .then_with(|| a.index.cmp(&b.index))
    });

    if let Some(best) = candidates.into_iter().next() {
        debug!("Found matching mock with score {}", best.score);
        Some(best)
    } else {
        debug!("No matching mock found");
        None
    }
}

/// Score awarded for a method + path match, before any matcher bonuses.
const BASE_SCORE: u32 = 1000;
/// Score added per declared query parameter.
const QUERY_PARAM_SCORE: u32 = 100;
/// Score added per declared required header.
const HEADER_SCORE: u32 = 50;
/// Score added for a satisfied body matcher.
const BODY_SCORE: u32 = 500;

/// How a mock's score was arrived at, kept so the dashboard can show the
/// arithmetic instead of an unexplained number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScoreBreakdown {
    pub base: u32,
    pub query_params: u32,
    pub headers: u32,
    pub body: u32,
}

impl ScoreBreakdown {
    fn total(&self) -> u32 {
        self.base + self.query_params + self.headers + self.body
    }
}

/// The outcome of running every one of a mock's matchers against a request.
#[derive(Debug, Clone)]
pub enum Evaluation {
    Matched(ScoreBreakdown),
    Rejected(RejectReason),
}

/// Run a mock's matchers against a request: the single place matching is
/// decided.
///
/// [`calculate_match_score`] (does it match, and how well?) and
/// [`explain_no_match`] (why didn't it?) are both views of this one function,
/// which is what keeps the dashboard's explanations honest.
pub fn evaluate_mock(context: &RequestContext, mock: &MockConfig) -> Evaluation {
    let mut breakdown = ScoreBreakdown {
        base: BASE_SCORE, // Base score for method + path match
        ..Default::default()
    };

    // Check query params
    if let Some(ref query_matcher) = mock.query_params {
        if let Some(reason) = query_params_reject_reason(&context.query_params, query_matcher) {
            return Evaluation::Rejected(reason);
        }
        // Add score based on number of matched params
        breakdown.query_params = (query_matcher.params.len() as u32) * QUERY_PARAM_SCORE;
    }

    // Check headers
    if let Some(ref header_matcher) = mock.headers {
        if let Some(reason) = headers_reject_reason(&context.headers, header_matcher) {
            return Evaluation::Rejected(reason);
        }
        // Add score based on number of matched headers
        breakdown.headers = (header_matcher.required.len() as u32) * HEADER_SCORE;
    }

    // Check body
    if let Some(ref body_matcher) = mock.body {
        if let Some(ref body_bytes) = context.body {
            let parsed_body = parse_body(body_bytes, context.content_type.as_deref());
            if let Some(reason) = body_reject_reason(&parsed_body, body_matcher) {
                return Evaluation::Rejected(reason);
            }
            // Body matching is worth the most
            breakdown.body = BODY_SCORE;
        } else if !matches!(body_matcher, BodyMatcher::Empty | BodyMatcher::Any) {
            // Body required but not present
            return Evaluation::Rejected(RejectReason::BodyAbsent);
        }
    }

    Evaluation::Matched(breakdown)
}

/// Calculate match score for a mock
/// Returns None if the mock doesn't match, Some(score) if it matches
fn calculate_match_score(context: &RequestContext, mock: &MockConfig) -> Option<ScoreBreakdown> {
    match evaluate_mock(context, mock) {
        Evaluation::Matched(breakdown) => Some(breakdown),
        Evaluation::Rejected(_) => None,
    }
}

// ============================================================================
// Match Explanations - why this mock, or why none
// ============================================================================

/// How a mock is named in an explanation: its source file when it was loaded
/// from one, otherwise its registration key and position.
fn describe_mock(mock: &MockConfig, key: &str, index: usize) -> String {
    match mock.source {
        Some(ref source) => source.clone(),
        None => format!("`{}` #{}", key, index),
    }
}

/// Explain, in one line, why the winning mock won.
///
/// Reads only the [`MatchResult`], whose breakdown was recorded when the score
/// was computed — nothing is re-derived here, so the arithmetic shown is
/// always the arithmetic that ran.
pub fn explain_match(result: &MatchResult) -> String {
    let mut parts = vec![format!("method+path {}", result.breakdown.base)];
    if result.breakdown.query_params > 0 {
        parts.push(format!("query params +{}", result.breakdown.query_params));
    }
    if result.breakdown.headers > 0 {
        parts.push(format!("headers +{}", result.breakdown.headers));
    }
    if result.breakdown.body > 0 {
        parts.push(format!("body +{}", result.breakdown.body));
    }
    // A pattern route always captures at least one parameter, so a non-empty
    // capture map is exactly the case the penalty was applied to.
    if !result.path_params.is_empty() {
        parts.push(format!("path pattern -{}", PATTERN_MATCH_PENALTY));
    }

    format!(
        "matched {} (score {}: {})",
        describe_mock(&result.mock, &result.matched_key, result.index),
        result.score,
        parts.join(", ")
    )
}

/// Longest list of near-miss candidates spelled out in a no-match explanation.
const MAX_EXPLAINED_CANDIDATES: usize = 5;

/// Explain why nothing matched: which mocks were even in the running, and the
/// first matcher that rejected each.
///
/// Candidates are gathered exactly the way [`find_matching_mock`] gathers
/// them — the mocks under the request's `METHOD:path` key, plus any pattern
/// route whose template the path fits — and each is judged by the same
/// [`evaluate_mock`] that produced the 404.
///
/// Meant to be called only on the 404 path: it walks the mock map and formats
/// strings, so the matched path never pays for it.
///
/// Mocks hidden by the scenario selection are reported as a count rather than
/// judged by their matchers — "3 mock(s) match this request but are filtered
/// out by inactive tags" is the answer somebody staring at an unexpected 404
/// actually needs. This string reaches the debug log and `/admin/requests`,
/// never the 404 body, so scenario configuration isn't leaked to API clients.
pub fn explain_no_match(
    context: &RequestContext,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> Option<String> {
    let base_key = crate::types::create_mock_key(&context.method, &context.path);
    let mut candidates: Vec<(String, RejectReason)> = Vec::new();
    let mut tag_filtered: Vec<String> = Vec::new();

    let mut collect = |key: &str, mock_list: &[MockConfig]| {
        for (index, mock) in mock_list.iter().enumerate() {
            if !mock.is_active(active_tags) {
                // Would this mock have matched if its tags were active? Only
                // then is it worth mentioning; a tagged mock whose matchers
                // reject the request is just a miss like any other.
                if matches!(evaluate_mock(context, mock), Evaluation::Matched(_)) {
                    tag_filtered.push(describe_mock(mock, key, index));
                }
                continue;
            }
            if let Evaluation::Rejected(reason) = evaluate_mock(context, mock) {
                candidates.push((describe_mock(mock, key, index), reason));
            }
        }
    };

    if let Some(mock_list) = mocks.get(&base_key) {
        collect(&base_key, mock_list);
    }

    // Pattern routes are candidates for this path too — sorted so the report
    // doesn't reshuffle with `HashMap` iteration order.
    let method_prefix = format!("{}:", context.method.to_uppercase());
    for key in sorted_keys(mocks) {
        if *key == base_key {
            continue;
        }
        let Some(path_part) = key.strip_prefix(method_prefix.as_str()) else {
            continue;
        };
        if !is_pattern_path(path_part) {
            continue;
        }
        let Some(pattern) = compile_path_pattern(path_part) else {
            continue;
        };
        if match_path_pattern(&pattern, &context.path).is_none() {
            continue;
        }
        if let Some(mock_list) = mocks.get(key) {
            collect(key, mock_list);
        }
    }

    // The scenario hint, built once and reused by both branches below.
    let tag_note = (!tag_filtered.is_empty()).then(|| {
        let shown: Vec<String> = tag_filtered
            .iter()
            .take(MAX_EXPLAINED_CANDIDATES)
            .cloned()
            .collect();
        let mut note = format!(
            "{} mock(s) match `{}` but are filtered out by inactive tags: {}",
            tag_filtered.len(),
            base_key,
            shown.join(", ")
        );
        if tag_filtered.len() > shown.len() {
            note.push_str(&format!(" (+{} more)", tag_filtered.len() - shown.len()));
        }
        note
    });

    if candidates.is_empty() {
        // A scenario hiding the only mock that would have served this request
        // is the whole explanation — say that instead of "nothing registered".
        if let Some(note) = tag_note {
            return Some(note);
        }

        // Nothing was even registered for this method+path. The most common
        // cause by far is the right path on the wrong method, so say so when
        // that's what happened.
        let other_methods = methods_registered_for_path(&context.path, mocks);
        return Some(if other_methods.is_empty() {
            format!("no mock is registered for `{}`", base_key)
        } else {
            format!(
                "no mock is registered for `{}` — `{}` is registered for {}",
                base_key,
                context.path,
                other_methods.join(", ")
            )
        });
    }

    let shown: Vec<String> = candidates
        .iter()
        .take(MAX_EXPLAINED_CANDIDATES)
        .map(|(name, reason)| format!("{} — {}", name, reason))
        .collect();

    let mut explanation = format!(
        "{} candidate mock(s) for `{}`, none matched: {}",
        candidates.len(),
        base_key,
        shown.join("; ")
    );
    if candidates.len() > shown.len() {
        explanation.push_str(&format!(" (+{} more)", candidates.len() - shown.len()));
    }
    if let Some(note) = tag_note {
        explanation.push_str(&format!("; {}", note));
    }
    Some(explanation)
}

/// The methods, sorted, that some mock is registered under for `path` —
/// the "you meant POST, didn't you?" hint.
fn methods_registered_for_path(
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
) -> Vec<String> {
    let mut methods: Vec<String> = mocks
        .keys()
        .filter_map(|key| {
            let (method, key_path) = key.split_once(':')?;
            (key_path == path).then(|| method.to_string())
        })
        .collect();
    methods.sort();
    methods.dedup();
    methods
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Query Parameter Tests
    #[test]
    fn test_parse_query_string() {
        let params = parse_query_string(Some("page=1&limit=10"));
        assert_eq!(params.get("page"), Some(&"1".to_string()));
        assert_eq!(params.get("limit"), Some(&"10".to_string()));
    }

    #[test]
    fn test_parse_query_string_url_encoded() {
        let params = parse_query_string(Some("q=hello%20world"));
        assert_eq!(params.get("q"), Some(&"hello world".to_string()));
    }

    #[test]
    fn test_parse_query_string_empty() {
        let params = parse_query_string(None);
        assert!(params.is_empty());

        let params = parse_query_string(Some(""));
        assert!(params.is_empty());
    }

    #[test]
    fn test_match_query_params_exact() {
        let matcher = QueryParamMatcher {
            params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
            strict: false,
        };

        let request = HashMap::from([("page".to_string(), "1".to_string())]);
        assert!(match_query_params(&request, &matcher));

        let request = HashMap::from([("page".to_string(), "2".to_string())]);
        assert!(!match_query_params(&request, &matcher));
    }

    #[test]
    fn test_match_query_params_extra_ignored() {
        let matcher = QueryParamMatcher {
            params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
            strict: false,
        };

        let request = HashMap::from([
            ("page".to_string(), "1".to_string()),
            ("extra".to_string(), "value".to_string()),
        ]);
        assert!(match_query_params(&request, &matcher));
    }

    #[test]
    fn test_match_query_params_strict() {
        let matcher = QueryParamMatcher {
            params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
            strict: true,
        };

        let request = HashMap::from([
            ("page".to_string(), "1".to_string()),
            ("extra".to_string(), "value".to_string()),
        ]);
        assert!(!match_query_params(&request, &matcher));
    }

    #[test]
    fn test_match_query_params_regex() {
        let matcher = QueryParamMatcher {
            params: HashMap::from([(
                "page".to_string(),
                QueryParamValue::Pattern(QueryParamPattern::Regex("^[0-9]+$".to_string())),
            )]),
            strict: false,
        };

        let request = HashMap::from([("page".to_string(), "123".to_string())]);
        assert!(match_query_params(&request, &matcher));

        let request = HashMap::from([("page".to_string(), "abc".to_string())]);
        assert!(!match_query_params(&request, &matcher));
    }

    #[test]
    fn test_match_query_params_any() {
        let matcher = QueryParamMatcher {
            params: HashMap::from([(
                "token".to_string(),
                QueryParamValue::Pattern(QueryParamPattern::Any),
            )]),
            strict: false,
        };

        let request = HashMap::from([("token".to_string(), "anything".to_string())]);
        assert!(match_query_params(&request, &matcher));
    }

    // Header Tests
    #[test]
    fn test_match_headers_exact() {
        let matcher = HeaderMatcher {
            required: HashMap::from([(
                "authorization".to_string(),
                HeaderValue::Exact("Bearer token".to_string()),
            )]),
            forbidden: vec![],
            strict: false,
        };

        let request = HashMap::from([("authorization".to_string(), "Bearer token".to_string())]);
        assert!(match_headers(&request, &matcher));
    }

    #[test]
    fn test_match_headers_case_insensitive() {
        let matcher = HeaderMatcher {
            required: HashMap::from([(
                "Authorization".to_string(),
                HeaderValue::Exact("Bearer token".to_string()),
            )]),
            forbidden: vec![],
            strict: false,
        };

        // Request headers are normalized to lowercase
        let request = HashMap::from([("authorization".to_string(), "Bearer token".to_string())]);
        assert!(match_headers(&request, &matcher));
    }

    #[test]
    fn test_match_headers_prefix() {
        let matcher = HeaderMatcher {
            required: HashMap::from([(
                "authorization".to_string(),
                HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
            )]),
            forbidden: vec![],
            strict: false,
        };

        let request = HashMap::from([("authorization".to_string(), "Bearer abc123".to_string())]);
        assert!(match_headers(&request, &matcher));

        let request = HashMap::from([("authorization".to_string(), "Basic abc123".to_string())]);
        assert!(!match_headers(&request, &matcher));
    }

    #[test]
    fn test_match_headers_forbidden() {
        let matcher = HeaderMatcher {
            required: HashMap::new(),
            forbidden: vec!["x-debug".to_string()],
            strict: false,
        };

        let request = HashMap::from([("x-debug".to_string(), "true".to_string())]);
        assert!(!match_headers(&request, &matcher));

        let request = HashMap::from([("x-other".to_string(), "value".to_string())]);
        assert!(match_headers(&request, &matcher));
    }

    // Body Tests
    #[test]
    fn test_match_json_exact() {
        let matcher = JsonBodyMatcher {
            exact: Some(serde_json::json!({"name": "Alice"})),
            partial: None,
            strict: false,
        };

        let actual = serde_json::json!({"name": "Alice"});
        assert!(match_json_body(&actual, &matcher));

        let wrong = serde_json::json!({"name": "Bob"});
        assert!(!match_json_body(&wrong, &matcher));
    }

    #[test]
    fn test_match_json_partial() {
        let matcher = JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({"name": "Alice"})),
            strict: false,
        };

        let actual = serde_json::json!({"name": "Alice", "age": 25});
        assert!(match_json_body(&actual, &matcher));
    }

    #[test]
    fn test_match_json_partial_strict() {
        let matcher = JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({"name": "Alice"})),
            strict: true,
        };

        let actual = serde_json::json!({"name": "Alice", "age": 25});
        assert!(!match_json_body(&actual, &matcher));

        let actual = serde_json::json!({"name": "Alice"});
        assert!(match_json_body(&actual, &matcher));
    }

    #[test]
    fn test_match_json_nested() {
        let matcher = JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({
                "user": {
                    "name": "Alice"
                }
            })),
            strict: false,
        };

        let actual = serde_json::json!({
            "user": {
                "name": "Alice",
                "email": "alice@example.com"
            },
            "meta": {}
        });
        assert!(match_json_body(&actual, &matcher));
    }

    #[test]
    fn test_match_text_contains() {
        let matcher = TextBodyMatcher {
            exact: None,
            contains: Some("hello".to_string()),
            regex: None,
        };

        assert!(match_text_body("hello world", &matcher));
        assert!(!match_text_body("goodbye", &matcher));
    }

    #[test]
    fn test_match_text_regex() {
        let matcher = TextBodyMatcher {
            exact: None,
            contains: None,
            regex: Some("^[A-Z]{3}-\\d{3}$".to_string()),
        };

        assert!(match_text_body("ABC-123", &matcher));
        assert!(!match_text_body("abc-123", &matcher));
    }

    #[test]
    fn test_match_form_body() {
        let matcher = FormBodyMatcher {
            fields: HashMap::from([("username".to_string(), "admin".to_string())]),
            strict: false,
        };

        let actual = HashMap::from([
            ("username".to_string(), "admin".to_string()),
            ("extra".to_string(), "value".to_string()),
        ]);
        assert!(match_form_body(&actual, &matcher));
    }

    #[test]
    fn test_match_form_body_strict() {
        let matcher = FormBodyMatcher {
            fields: HashMap::from([("username".to_string(), "admin".to_string())]),
            strict: true,
        };

        let actual = HashMap::from([
            ("username".to_string(), "admin".to_string()),
            ("extra".to_string(), "value".to_string()),
        ]);
        assert!(!match_form_body(&actual, &matcher));
    }

    #[test]
    fn test_parse_body_json() {
        let bytes = Bytes::from(r#"{"name":"Alice"}"#);
        let parsed = parse_body(&bytes, Some("application/json"));

        if let ParsedBody::Json(json) = parsed {
            assert_eq!(json["name"], "Alice");
        } else {
            panic!("Expected JSON body");
        }
    }

    #[test]
    fn test_parse_body_form() {
        let bytes = Bytes::from("username=admin&password=secret");
        let parsed = parse_body(&bytes, Some("application/x-www-form-urlencoded"));

        if let ParsedBody::Form(form) = parsed {
            assert_eq!(form.get("username"), Some(&"admin".to_string()));
            assert_eq!(form.get("password"), Some(&"secret".to_string()));
        } else {
            panic!("Expected Form body");
        }
    }

    #[test]
    fn test_parse_body_empty() {
        let bytes = Bytes::new();
        let parsed = parse_body(&bytes, Some("application/json"));

        assert!(matches!(parsed, ParsedBody::Empty));
    }

    #[test]
    fn test_find_matching_mock_returns_index() {
        let make_mock = |role: &str| MockConfig {
            method: "POST".to_string(),
            path: "/login".to_string(),
            status: 200,
            response: serde_json::json!({"role": role}),
            consume_body: true,
            query_params: None,
            headers: None,
            body: Some(BodyMatcher::Json(JsonBodyMatcher {
                exact: None,
                partial: Some(serde_json::json!({"role": role})),
                strict: false,
            })),
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: Vec::new(),
        };

        let mocks = HashMap::from([(
            "POST:/login".to_string(),
            vec![make_mock("admin"), make_mock("user")],
        )]);

        let make_context = |role: &str| RequestContext {
            method: "POST".to_string(),
            path: "/login".to_string(),
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            headers: HashMap::new(),
            body: Some(Bytes::from(format!(r#"{{"role":"{}"}}"#, role))),
            content_type: Some("application/json".to_string()),
        };

        let result = find_matching_mock(&make_context("admin"), &mocks, None).unwrap();
        assert_eq!(result.index, 0);
        assert_eq!(result.mock.response["role"], "admin");
        assert!(result.path_params.is_empty());
        assert_eq!(result.matched_key, "POST:/login");

        let result = find_matching_mock(&make_context("user"), &mocks, None).unwrap();
        assert_eq!(result.index, 1);
        assert_eq!(result.mock.response["role"], "user");
    }

    // ========================================================================
    // Path Parameter Tests
    // ========================================================================

    #[test]
    fn test_is_pattern_path() {
        assert!(is_pattern_path("/users/:id"));
        assert!(is_pattern_path("/orgs/{org}/repos/{repo}"));
        assert!(is_pattern_path("/users/:id/posts/:postId"));
        assert!(!is_pattern_path("/users/123"));
        assert!(!is_pattern_path("/users"));
        assert!(!is_pattern_path("/"));
    }

    #[test]
    fn test_compile_path_pattern_single_param_colon_style() {
        let pattern = compile_path_pattern("/users/:id").unwrap();
        assert_eq!(pattern.param_names, vec!["id".to_string()]);
        assert!(pattern.regex.is_match("/users/42"));
        assert!(!pattern.regex.is_match("/users/42/extra"));
        assert!(!pattern.regex.is_match("/users"));
    }

    #[test]
    fn test_compile_path_pattern_single_param_brace_style() {
        let pattern = compile_path_pattern("/users/{id}").unwrap();
        assert_eq!(pattern.param_names, vec!["id".to_string()]);
        assert!(pattern.regex.is_match("/users/42"));
    }

    #[test]
    fn test_compile_path_pattern_multiple_params() {
        let pattern = compile_path_pattern("/orgs/{org}/repos/{repo}").unwrap();
        assert_eq!(
            pattern.param_names,
            vec!["org".to_string(), "repo".to_string()]
        );
        assert!(pattern.regex.is_match("/orgs/acme/repos/widgets"));
    }

    #[test]
    fn test_compile_path_pattern_nested_mixed_literal_segments() {
        let pattern = compile_path_pattern("/orders/:id/items/:itemId").unwrap();
        assert_eq!(
            pattern.param_names,
            vec!["id".to_string(), "itemId".to_string()]
        );
        assert!(pattern.regex.is_match("/orders/ORD-9981/items/2"));
        assert!(!pattern.regex.is_match("/orders/ORD-9981"));
    }

    #[test]
    fn test_compile_path_pattern_returns_none_without_params() {
        assert!(compile_path_pattern("/users/123").is_none());
        assert!(compile_path_pattern("/users").is_none());
    }

    #[test]
    fn test_compile_path_pattern_escapes_regex_metacharacters_in_literals() {
        // A literal segment that looks like a regex special char shouldn't
        // be interpreted as one.
        let pattern = compile_path_pattern("/a.b/:id").unwrap();
        assert!(pattern.regex.is_match("/a.b/1"));
        assert!(!pattern.regex.is_match("/aXb/1"));
    }

    #[test]
    fn test_match_path_pattern_captures_values() {
        let pattern = compile_path_pattern("/orgs/{org}/repos/{repo}").unwrap();
        let params = match_path_pattern(&pattern, "/orgs/acme/repos/widgets").unwrap();
        assert_eq!(params.get("org"), Some(&"acme".to_string()));
        assert_eq!(params.get("repo"), Some(&"widgets".to_string()));
    }

    #[test]
    fn test_match_path_pattern_no_match_returns_none() {
        let pattern = compile_path_pattern("/users/:id").unwrap();
        assert!(match_path_pattern(&pattern, "/posts/1").is_none());
        assert!(match_path_pattern(&pattern, "/users/1/extra").is_none());
    }

    fn path_param_mock(path: &str, response: serde_json::Value) -> MockConfig {
        MockConfig {
            method: "GET".to_string(),
            path: path.to_string(),
            status: 200,
            response,
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: Vec::new(),
        }
    }

    fn path_param_context(path: &str) -> RequestContext {
        RequestContext {
            method: "GET".to_string(),
            path: path.to_string(),
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            content_type: None,
        }
    }

    #[test]
    fn test_find_matching_mock_single_path_param() {
        let mocks = HashMap::from([(
            "GET:/users/:id".to_string(),
            vec![path_param_mock(
                "/users/:id",
                serde_json::json!({"name": "Mock User"}),
            )],
        )]);

        let result = find_matching_mock(&path_param_context("/users/42"), &mocks, None).unwrap();
        assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
        assert_eq!(result.matched_key, "GET:/users/:id");
        // Below the 1000 base score of an exact match
        assert!(result.score < 1000);
    }

    #[test]
    fn test_find_matching_mock_multiple_path_params() {
        let mocks = HashMap::from([(
            "DELETE:/orgs/{org}/repos/{repo}".to_string(),
            vec![MockConfig {
                method: "DELETE".to_string(),
                ..path_param_mock("/orgs/{org}/repos/{repo}", serde_json::json!(null))
            }],
        )]);

        let context = RequestContext {
            method: "DELETE".to_string(),
            path: "/orgs/acme/repos/widgets".to_string(),
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            content_type: None,
        };

        let result = find_matching_mock(&context, &mocks, None).unwrap();
        assert_eq!(result.path_params.get("org"), Some(&"acme".to_string()));
        assert_eq!(result.path_params.get("repo"), Some(&"widgets".to_string()));
    }

    #[test]
    fn test_find_matching_mock_nested_path_params() {
        let mocks = HashMap::from([(
            "GET:/orders/:id/items/:itemId".to_string(),
            vec![path_param_mock(
                "/orders/:id/items/:itemId",
                serde_json::json!({"ok": true}),
            )],
        )]);

        let result = find_matching_mock(
            &path_param_context("/orders/ORD-9981/items/2"),
            &mocks,
            None,
        )
        .unwrap();
        assert_eq!(result.path_params.get("id"), Some(&"ORD-9981".to_string()));
        assert_eq!(result.path_params.get("itemId"), Some(&"2".to_string()));
    }

    #[test]
    fn test_find_matching_mock_exact_path_beats_pattern() {
        let mocks = HashMap::from([
            (
                "GET:/users/:id".to_string(),
                vec![path_param_mock(
                    "/users/:id",
                    serde_json::json!({"source": "pattern"}),
                )],
            ),
            (
                "GET:/users/42".to_string(),
                vec![path_param_mock(
                    "/users/42",
                    serde_json::json!({"source": "exact"}),
                )],
            ),
        ]);

        let result = find_matching_mock(&path_param_context("/users/42"), &mocks, None).unwrap();
        assert_eq!(result.mock.response["source"], "exact");
        assert!(result.path_params.is_empty());

        // A different id has no exact mock, so the pattern still matches
        let result = find_matching_mock(&path_param_context("/users/7"), &mocks, None).unwrap();
        assert_eq!(result.mock.response["source"], "pattern");
        assert_eq!(result.path_params.get("id"), Some(&"7".to_string()));
    }

    #[test]
    fn test_find_matching_mock_pattern_no_match_returns_none() {
        let mocks = HashMap::from([(
            "GET:/users/:id".to_string(),
            vec![path_param_mock(
                "/users/:id",
                serde_json::json!({"name": "Mock User"}),
            )],
        )]);

        assert!(find_matching_mock(&path_param_context("/posts/1"), &mocks, None).is_none());
    }

    // ------------------------------------------------------------------
    // Compilation caching / pattern short-circuit
    // ------------------------------------------------------------------

    fn plain_mock(method: &str, path: &str, marker: &str) -> MockConfig {
        MockConfig {
            method: method.to_string(),
            path: path.to_string(),
            status: 200,
            response: serde_json::json!({"source": marker}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: Vec::new(),
        }
    }

    fn mock_map(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for mock in mocks {
            map.entry(crate::types::create_mock_key(&mock.method, &mock.path))
                .or_default()
                .push(mock);
        }
        map
    }

    #[test]
    fn test_path_pattern_compiled_once_across_many_requests() {
        // A template unique to this test, so the shared process-wide cache
        // can't have been populated by another test.
        let template = "/cache-probe-once/:id";
        assert!(!path_pattern_is_cached(template));

        let mocks = mock_map(vec![plain_mock("GET", template, "pattern")]);

        for i in 0..50 {
            let mut ctx =
                RequestContext::new("GET".to_string(), format!("/cache-probe-once/{}", i));
            ctx.path_params = HashMap::new();
            let result = find_matching_mock(&ctx, &mocks, None).expect("pattern should match");
            assert_eq!(result.mock.response["source"], "pattern");
        }

        // Compilation is memoized by template string, so the 50 requests above
        // share the single entry created by the first one.
        assert!(path_pattern_is_cached(template));
        let first = compile_path_pattern(template).unwrap();
        let second = compile_path_pattern(template).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn test_exact_match_never_compiles_pattern_routes() {
        let template = "/cache-probe-skip/:id";
        assert!(!path_pattern_is_cached(template));

        let mocks = mock_map(vec![
            plain_mock("GET", "/cache-probe-skip/42", "exact"),
            plain_mock("GET", template, "pattern"),
        ]);

        let ctx = RequestContext::new("GET".to_string(), "/cache-probe-skip/42".to_string());
        let result = find_matching_mock(&ctx, &mocks, None).expect("exact should match");

        assert_eq!(result.mock.response["source"], "exact");
        assert!(result.path_params.is_empty());
        // The pattern loop never ran, so its template was never compiled.
        assert!(
            !path_pattern_is_cached(template),
            "exact-match request reached the path-pattern compiler"
        );
    }

    #[test]
    fn test_pattern_still_used_when_exact_key_exists_but_does_not_match() {
        let template = "/cache-probe-fallthrough/:id";
        let mut exact = plain_mock("GET", "/cache-probe-fallthrough/42", "exact");
        exact.query_params = Some(QueryParamMatcher {
            params: HashMap::from([(
                "required".to_string(),
                QueryParamValue::Exact("yes".to_string()),
            )]),
            strict: false,
        });

        let mocks = mock_map(vec![exact, plain_mock("GET", template, "pattern")]);

        // The exact key exists but its query matcher rejects the request, so
        // the pattern scan must still run.
        let ctx = RequestContext::new("GET".to_string(), "/cache-probe-fallthrough/42".to_string());
        let result = find_matching_mock(&ctx, &mocks, None).expect("pattern should match");

        assert_eq!(result.mock.response["source"], "pattern");
        assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
    }

    #[test]
    fn test_matcher_regex_is_compiled_once_per_pattern() {
        let pattern = "^probe-[a-z]+$";
        let matcher = HeaderMatcher {
            required: HashMap::from([(
                "x-probe".to_string(),
                HeaderValue::Pattern(HeaderPattern::Regex(pattern.to_string())),
            )]),
            forbidden: Vec::new(),
            strict: false,
        };

        let mut headers = HashMap::new();
        headers.insert("x-probe".to_string(), "probe-value".to_string());

        for _ in 0..100 {
            assert!(match_headers(&headers, &matcher));
        }

        // Every one of those calls resolved to the same compiled handle.
        let a = crate::regex_cache::compiled_regex(pattern).unwrap();
        let b = crate::regex_cache::compiled_regex(pattern).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn test_invalid_matcher_regex_never_matches() {
        let matcher = HeaderMatcher {
            required: HashMap::from([(
                "x-bad".to_string(),
                HeaderValue::Pattern(HeaderPattern::Regex("([unclosed".to_string())),
            )]),
            forbidden: Vec::new(),
            strict: false,
        };

        let mut headers = HashMap::new();
        headers.insert("x-bad".to_string(), "anything".to_string());

        assert!(!match_headers(&headers, &matcher));
    }
    // ------------------------------------------------------------------
    // Deterministic tie-breaking (#55)
    // ------------------------------------------------------------------

    #[test]
    fn test_equal_score_patterns_resolve_identically_regardless_of_insertion_order() {
        let specific = plain_mock("GET", "/users/:id", "specific");
        let generic = plain_mock("GET", "/{resource}/:id", "generic");

        // Same two mocks, opposite insertion order. HashMap iteration order is
        // reseeded per map, so this is exactly the shape that used to flip.
        let forward = mock_map(vec![specific.clone(), generic.clone()]);
        let reverse = mock_map(vec![generic, specific]);

        let ctx = RequestContext::new("GET".to_string(), "/users/42".to_string());

        let a = find_matching_mock(&ctx, &forward, None).expect("should match");
        let b = find_matching_mock(&ctx, &reverse, None).expect("should match");

        assert_eq!(a.matched_key, b.matched_key);
        assert_eq!(a.mock.response["source"], b.mock.response["source"]);
        // More literal segments wins: /users/:id over /{resource}/:id.
        assert_eq!(a.mock.response["source"], "specific");
    }

    #[test]
    fn test_equal_specificity_tie_breaks_on_key_lexicographically() {
        // Both have zero literal segments, so the key decides.
        let alpha = plain_mock("GET", "/{alpha}/:sub", "alpha");
        let beta = plain_mock("GET", "/{beta}/:sub", "beta");

        let forward = mock_map(vec![alpha.clone(), beta.clone()]);
        let reverse = mock_map(vec![beta, alpha]);

        let ctx = RequestContext::new("GET".to_string(), "/anything/42".to_string());

        let a = find_matching_mock(&ctx, &forward, None).expect("should match");
        let b = find_matching_mock(&ctx, &reverse, None).expect("should match");

        assert_eq!(a.matched_key, b.matched_key);
        assert_eq!(a.mock.response["source"], "alpha");
    }

    #[test]
    fn test_repeated_lookups_are_stable_across_freshly_built_maps() {
        let specific = plain_mock("GET", "/orders/:id", "specific");
        let generic = plain_mock("GET", "/{resource}/:id", "generic");
        let ctx = RequestContext::new("GET".to_string(), "/orders/7".to_string());

        // Rebuilding the map is what a hot reload does; each rebuild gets a
        // fresh hasher seed.
        let winner = {
            let map = mock_map(vec![specific.clone(), generic.clone()]);
            find_matching_mock(&ctx, &map, None).unwrap().matched_key
        };

        for _ in 0..25 {
            let map = mock_map(vec![specific.clone(), generic.clone()]);
            assert_eq!(
                find_matching_mock(&ctx, &map, None).unwrap().matched_key,
                winner
            );
        }
    }

    #[test]
    fn test_path_specificity_counts_literal_segments_only() {
        assert_eq!(path_specificity("/users/42"), 2);
        assert_eq!(path_specificity("/users/:id"), 1);
        assert_eq!(path_specificity("/{resource}/:id"), 0);
        assert_eq!(path_specificity("/orgs/{org}/repos/{repo}"), 2);
    }

    // ------------------------------------------------------------------
    // Strict header mode ignores client-default headers (#56)
    // ------------------------------------------------------------------

    fn strict_bearer_matcher() -> HeaderMatcher {
        HeaderMatcher {
            required: HashMap::from([(
                "authorization".to_string(),
                HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
            )]),
            forbidden: Vec::new(),
            strict: true,
        }
    }

    #[test]
    fn test_strict_mode_allows_implicit_accept_header() {
        // What `curl -H "Authorization: Bearer abc" ...` actually sends.
        let headers = HashMap::from([
            ("host".to_string(), "localhost:8080".to_string()),
            ("user-agent".to_string(), "curl/8.5.0".to_string()),
            ("accept".to_string(), "*/*".to_string()),
            ("authorization".to_string(), "Bearer abc123".to_string()),
        ]);

        assert!(match_headers(&headers, &strict_bearer_matcher()));
    }

    #[test]
    fn test_strict_mode_ignores_every_documented_default_header() {
        let mut headers = HashMap::from([("authorization".to_string(), "Bearer abc".to_string())]);
        for ignored in IGNORED_HEADERS {
            headers.insert(ignored.to_string(), "value".to_string());
        }

        assert!(match_headers(&headers, &strict_bearer_matcher()));
    }

    #[test]
    fn test_strict_mode_still_rejects_genuinely_extra_headers() {
        let headers = HashMap::from([
            ("accept".to_string(), "*/*".to_string()),
            ("authorization".to_string(), "Bearer abc123".to_string()),
            ("x-unexpected".to_string(), "surprise".to_string()),
        ]);

        assert!(!match_headers(&headers, &strict_bearer_matcher()));
    }
}

/// Coarse micro-benchmarks backing the numbers in `ADVANCED_MATCHING.md`'s
/// "Performance Notes". Excluded from the normal suite; reproduce with:
///
/// ```text
/// cargo test --release -- --ignored --nocapture
/// ```
#[cfg(test)]
mod benches {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn bench_regex_matcher() {
        let pattern = "^[A-Za-z0-9]{32}$";
        let value = "abcdefghijklmnopqrstuvwxyz012345";
        let n = 100_000;

        let start = Instant::now();
        for _ in 0..n {
            let re = Regex::new(pattern).unwrap();
            assert!(re.is_match(value));
        }
        let uncached = start.elapsed();

        let start = Instant::now();
        for _ in 0..n {
            let re = compiled_regex(pattern).unwrap();
            assert!(re.is_match(value));
        }
        let cached_time = start.elapsed();

        println!(
            "regex: uncached {:?}/op, cached {:?}/op",
            uncached / n,
            cached_time / n
        );
    }

    #[test]
    #[ignore]
    fn bench_path_pattern() {
        let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for i in 0..20 {
            let path = format!("/bench{}/:id", i);
            mocks.insert(
                format!("GET:{}", path),
                vec![MockConfig {
                    method: "GET".to_string(),
                    path,
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
                }],
            );
        }
        mocks.insert(
            "GET:/bench-exact".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/bench-exact".to_string(),
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
            }],
        );

        let n = 50_000;
        let ctx = RequestContext::new("GET".to_string(), "/bench-exact".to_string());
        let start = Instant::now();
        for _ in 0..n {
            assert!(find_matching_mock(&ctx, &mocks, None).is_some());
        }
        println!(
            "exact match with 20 pattern routes loaded: {:?}/op",
            start.elapsed() / n
        );

        let ctx = RequestContext::new("GET".to_string(), "/bench19/42".to_string());
        let start = Instant::now();
        for _ in 0..n {
            assert!(find_matching_mock(&ctx, &mocks, None).is_some());
        }
        println!(
            "pattern match with 20 pattern routes loaded: {:?}/op",
            start.elapsed() / n
        );

        // Approximates the old per-request cost: recompiling all 20 templates.
        let templates: Vec<String> = (0..20).map(|i| format!("/bench{}/:id", i)).collect();
        let start = Instant::now();
        for _ in 0..n {
            for t in &templates {
                assert!(build_path_pattern(t).is_some());
            }
        }
        println!(
            "recompiling 20 path templates: {:?}/op",
            start.elapsed() / n
        );
    }
}

// ============================================================================
// Match Explanation Tests
// ============================================================================

#[cfg(test)]
mod explanation_tests {
    use super::*;
    use crate::types::{FormBodyMatcher, TextBodyMatcher};

    /// A bare mock with no matchers, which every request matches.
    fn plain_mock(method: &str, path: &str) -> MockConfig {
        MockConfig {
            method: method.to_string(),
            path: path.to_string(),
            status: 200,
            response: serde_json::json!({"ok": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: Vec::new(),
        }
    }

    fn ctx(method: &str, path: &str) -> RequestContext {
        RequestContext::new(method.to_string(), path.to_string())
    }

    fn store(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for mock in mocks {
            let key = crate::types::create_mock_key(&mock.method, &mock.path);
            map.entry(key).or_default().push(mock);
        }
        map
    }

    fn header_matcher(name: &str, value: HeaderValue) -> HeaderMatcher {
        let mut required = HashMap::new();
        required.insert(name.to_string(), value);
        HeaderMatcher {
            required,
            forbidden: Vec::new(),
            strict: false,
        }
    }

    // ── explain_no_match: nothing registered ────────────────────────────

    #[test]
    fn explain_no_match_reports_an_unregistered_route() {
        let mocks = store(vec![plain_mock("GET", "/users")]);
        let explanation = explain_no_match(&ctx("GET", "/orders"), &mocks, None).unwrap();
        assert!(
            explanation.contains("no mock is registered for `GET:/orders`"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_points_at_the_other_method_on_the_same_path() {
        // The most common 404 by far: right path, wrong verb.
        let mocks = store(vec![
            plain_mock("POST", "/users"),
            plain_mock("PUT", "/users"),
        ]);
        let explanation = explain_no_match(&ctx("GET", "/users"), &mocks, None).unwrap();
        assert!(explanation.contains("no mock is registered for `GET:/users`"));
        assert!(explanation.contains("POST"), "{}", explanation);
        assert!(explanation.contains("PUT"), "{}", explanation);
    }

    // ── explain_no_match: one case per matcher kind ─────────────────────

    #[test]
    fn explain_no_match_names_an_absent_required_header() {
        let mut mock = plain_mock("GET", "/users");
        mock.source = Some("mocks/get_users.json".to_string());
        mock.headers = Some(header_matcher(
            "x-api-key",
            HeaderValue::Exact("secret".to_string()),
        ));

        let explanation =
            explain_no_match(&ctx("GET", "/users"), &store(vec![mock]), None).unwrap();
        assert!(explanation.contains("1 candidate mock(s) for `GET:/users`"));
        assert!(
            explanation.contains("mocks/get_users.json"),
            "{}",
            explanation
        );
        assert!(
            explanation.contains("required header `x-api-key` was absent"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_names_a_mismatched_header_value() {
        let mut mock = plain_mock("GET", "/users");
        mock.headers = Some(header_matcher(
            "x-tenant",
            HeaderValue::Exact("acme".to_string()),
        ));

        let mut context = ctx("GET", "/users");
        context
            .headers
            .insert("x-tenant".to_string(), "globex".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("header `x-tenant` was `globex`"),
            "{}",
            explanation
        );
        assert!(explanation.contains("expected `acme`"), "{}", explanation);
    }

    #[test]
    fn explain_no_match_names_a_forbidden_header() {
        let mut mock = plain_mock("GET", "/users");
        mock.headers = Some(HeaderMatcher {
            required: HashMap::new(),
            forbidden: vec!["x-debug".to_string()],
            strict: false,
        });

        let mut context = ctx("GET", "/users");
        context
            .headers
            .insert("x-debug".to_string(), "1".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("forbidden header `x-debug` was present"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_names_an_absent_query_param() {
        let mut mock = plain_mock("GET", "/users");
        let mut params = HashMap::new();
        params.insert("page".to_string(), QueryParamValue::Exact("1".to_string()));
        mock.query_params = Some(QueryParamMatcher {
            params,
            strict: false,
        });

        let explanation =
            explain_no_match(&ctx("GET", "/users"), &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("required query param `page` was absent"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_describes_a_query_param_pattern() {
        let mut mock = plain_mock("GET", "/users");
        let mut params = HashMap::new();
        params.insert(
            "page".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::Regex("^[0-9]+$".to_string())),
        );
        mock.query_params = Some(QueryParamMatcher {
            params,
            strict: false,
        });

        let mut context = ctx("GET", "/users");
        context
            .query_params
            .insert("page".to_string(), "abc".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("query param `page` was `abc`"),
            "{}",
            explanation
        );
        assert!(explanation.contains("^[0-9]+$"), "{}", explanation);
    }

    #[test]
    fn explain_no_match_reports_a_missing_body() {
        let mut mock = plain_mock("POST", "/login");
        mock.body = Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({"user": "alice"})),
            strict: false,
        }));

        let explanation =
            explain_no_match(&ctx("POST", "/login"), &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("requires a request body and none was sent"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_names_the_missing_json_field() {
        let mut mock = plain_mock("POST", "/login");
        mock.body = Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({"user": {"name": "alice"}})),
            strict: false,
        }));

        let mut context = ctx("POST", "/login");
        context.body = Some(Bytes::from(r#"{"user":{"id":1}}"#));
        context.content_type = Some("application/json".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("missing the field `user.name`"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_names_the_mismatched_json_field() {
        let mut mock = plain_mock("POST", "/login");
        mock.body = Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({"role": "admin"})),
            strict: false,
        }));

        let mut context = ctx("POST", "/login");
        context.body = Some(Bytes::from(r#"{"role":"guest"}"#));
        context.content_type = Some("application/json".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("field `role` has a different value"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_reports_a_text_body_substring() {
        let mut mock = plain_mock("POST", "/notes");
        mock.body = Some(BodyMatcher::Text(TextBodyMatcher {
            exact: None,
            contains: Some("urgent".to_string()),
            regex: None,
        }));

        let mut context = ctx("POST", "/notes");
        context.body = Some(Bytes::from("just a note"));
        context.content_type = Some("text/plain".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("does not contain `urgent`"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_reports_a_form_field() {
        let mut mock = plain_mock("POST", "/form");
        let mut fields = HashMap::new();
        fields.insert("plan".to_string(), "pro".to_string());
        mock.body = Some(BodyMatcher::Form(FormBodyMatcher {
            fields,
            strict: false,
        }));

        let mut context = ctx("POST", "/form");
        context.body = Some(Bytes::from("plan=free"));
        context.content_type = Some("application/x-www-form-urlencoded".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("form body field `plan` has a different value"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_reports_an_expected_empty_body() {
        let mut mock = plain_mock("POST", "/ping");
        mock.body = Some(BodyMatcher::Empty);

        let mut context = ctx("POST", "/ping");
        context.body = Some(Bytes::from("nope"));
        context.content_type = Some("text/plain".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("requires an empty body"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_covers_pattern_routes() {
        // `/users/42` never matches an exact key, so the candidate has to be
        // found through the same pattern scan the matcher itself uses.
        let mut mock = plain_mock("GET", "/users/:id");
        mock.source = Some("mocks/get_user.json".to_string());
        mock.headers = Some(header_matcher(
            "x-api-key",
            HeaderValue::Pattern(HeaderPattern::Any),
        ));

        let explanation =
            explain_no_match(&ctx("GET", "/users/42"), &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains("mocks/get_user.json"),
            "{}",
            explanation
        );
        assert!(
            explanation.contains("required header `x-api-key` was absent"),
            "{}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_lists_every_candidate() {
        let mut a = plain_mock("GET", "/users");
        a.source = Some("mocks/a.json".to_string());
        a.headers = Some(header_matcher(
            "x-a",
            HeaderValue::Pattern(HeaderPattern::Any),
        ));
        let mut b = plain_mock("GET", "/users");
        b.source = Some("mocks/b.json".to_string());
        b.headers = Some(header_matcher(
            "x-b",
            HeaderValue::Pattern(HeaderPattern::Any),
        ));

        let explanation =
            explain_no_match(&ctx("GET", "/users"), &store(vec![a, b]), None).unwrap();
        assert!(
            explanation.starts_with("2 candidate mock(s)"),
            "{}",
            explanation
        );
        assert!(explanation.contains("mocks/a.json"), "{}", explanation);
        assert!(explanation.contains("mocks/b.json"), "{}", explanation);
    }

    #[test]
    fn explain_no_match_caps_the_candidate_list() {
        let mocks: Vec<MockConfig> = (0..8)
            .map(|i| {
                let mut mock = plain_mock("GET", "/users");
                mock.source = Some(format!("mocks/{}.json", i));
                mock.headers = Some(header_matcher(
                    &format!("x-{}", i),
                    HeaderValue::Pattern(HeaderPattern::Any),
                ));
                mock
            })
            .collect();

        let explanation = explain_no_match(&ctx("GET", "/users"), &store(mocks), None).unwrap();
        assert!(
            explanation.starts_with("8 candidate mock(s)"),
            "{}",
            explanation
        );
        assert!(explanation.contains("(+3 more)"), "{}", explanation);
    }

    #[test]
    fn explain_no_match_falls_back_to_the_key_when_a_mock_has_no_source() {
        // Single-file and in-process mocks have no source file to name.
        let mut mock = plain_mock("GET", "/users");
        mock.headers = Some(header_matcher(
            "x-api-key",
            HeaderValue::Pattern(HeaderPattern::Any),
        ));

        let explanation =
            explain_no_match(&ctx("GET", "/users"), &store(vec![mock]), None).unwrap();
        assert!(explanation.contains("`GET:/users` #0"), "{}", explanation);
    }

    // ── redaction ───────────────────────────────────────────────────────

    #[test]
    fn explanations_never_quote_a_sensitive_header_value() {
        for name in ["authorization", "cookie", "set-cookie"] {
            let mut mock = plain_mock("GET", "/vault");
            mock.headers = Some(header_matcher(
                name,
                HeaderValue::Exact("Bearer super-secret-token".to_string()),
            ));

            let mut context = ctx("GET", "/vault");
            context
                .headers
                .insert(name.to_string(), "Bearer leaked-value".to_string());

            let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
            assert!(
                explanation.contains(name),
                "the header name is still useful: {}",
                explanation
            );
            assert!(
                !explanation.contains("super-secret-token"),
                "expected value leaked: {}",
                explanation
            );
            assert!(
                !explanation.contains("leaked-value"),
                "request value leaked: {}",
                explanation
            );
            assert!(explanation.contains("[REDACTED]"), "{}", explanation);
        }
    }

    #[test]
    fn redaction_of_header_names_is_case_insensitive() {
        let mut mock = plain_mock("GET", "/vault");
        mock.headers = Some(header_matcher(
            "Authorization",
            HeaderValue::Exact("Bearer super-secret-token".to_string()),
        ));

        let mut context = ctx("GET", "/vault");
        context
            .headers
            .insert("authorization".to_string(), "Bearer nope".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            !explanation.contains("super-secret-token"),
            "{}",
            explanation
        );
        assert!(explanation.contains("[REDACTED]"), "{}", explanation);
    }

    // ── stability and bounds ────────────────────────────────────────────

    #[test]
    fn explanations_are_stable_across_runs() {
        // Two failing headers on one mock: whichever is reported must not
        // depend on HashMap iteration order, or screenshots and diffs of the
        // dashboard become worthless.
        let mut required = HashMap::new();
        required.insert(
            "x-zebra".to_string(),
            HeaderValue::Pattern(HeaderPattern::Any),
        );
        required.insert(
            "x-alpha".to_string(),
            HeaderValue::Pattern(HeaderPattern::Any),
        );
        let mut mock = plain_mock("GET", "/users");
        mock.headers = Some(HeaderMatcher {
            required,
            forbidden: Vec::new(),
            strict: false,
        });
        let mocks = store(vec![mock]);

        let first = explain_no_match(&ctx("GET", "/users"), &mocks, None).unwrap();
        for _ in 0..20 {
            assert_eq!(
                first,
                explain_no_match(&ctx("GET", "/users"), &mocks, None).unwrap()
            );
        }
        assert!(first.contains("x-alpha"), "{}", first);
    }

    #[test]
    fn long_values_are_truncated_in_explanations() {
        let long = "v".repeat(500);
        let mut mock = plain_mock("GET", "/users");
        mock.headers = Some(header_matcher(
            "x-long",
            HeaderValue::Exact("expected".to_string()),
        ));

        let mut context = ctx("GET", "/users");
        context.headers.insert("x-long".to_string(), long.clone());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            !explanation.contains(&long),
            "the full value was echoed back"
        );
        assert!(explanation.contains('…'), "{}", explanation);
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // A cut mid-character would produce invalid UTF-8; `truncate_for_display`
        // counts characters, so this must round-trip.
        let value = "é".repeat(200);
        let truncated = truncate_for_display(&value);
        assert!(truncated.chars().count() <= MAX_DISPLAY_LEN + 1);
        assert!(truncated.ends_with('…'));
    }

    // ── the matched case ────────────────────────────────────────────────

    #[test]
    fn explain_match_shows_the_score_arithmetic() {
        let mut mock = plain_mock("GET", "/users");
        mock.source = Some("mocks/get_users.json".to_string());
        mock.headers = Some(header_matcher(
            "x-api-key",
            HeaderValue::Pattern(HeaderPattern::Any),
        ));

        let mut context = ctx("GET", "/users");
        context
            .headers
            .insert("x-api-key".to_string(), "abc".to_string());

        let result = find_matching_mock(&context, &store(vec![mock]), None).unwrap();
        let explanation = explain_match(&result);

        assert_eq!(result.score, 1050);
        assert!(
            explanation.contains("matched mocks/get_users.json"),
            "{}",
            explanation
        );
        assert!(explanation.contains("score 1050"), "{}", explanation);
        assert!(explanation.contains("method+path 1000"), "{}", explanation);
        assert!(explanation.contains("headers +50"), "{}", explanation);
    }

    #[test]
    fn explain_match_notes_the_path_pattern_penalty() {
        let mock = plain_mock("GET", "/users/:id");
        let result =
            find_matching_mock(&ctx("GET", "/users/42"), &store(vec![mock]), None).unwrap();
        let explanation = explain_match(&result);

        assert_eq!(result.score, 900);
        assert!(explanation.contains("path pattern -100"), "{}", explanation);
        assert_eq!(result.path_params.get("id").map(String::as_str), Some("42"));
    }

    #[test]
    fn the_breakdown_always_adds_up_to_the_score() {
        // The breakdown is what the dashboard shows; if it ever stopped summing
        // to the score the explanation would be quietly lying.
        let mut mock = plain_mock("POST", "/login");
        let mut params = HashMap::new();
        params.insert("v".to_string(), QueryParamValue::Exact("2".to_string()));
        mock.query_params = Some(QueryParamMatcher {
            params,
            strict: false,
        });
        mock.headers = Some(header_matcher(
            "x-api-key",
            HeaderValue::Pattern(HeaderPattern::Any),
        ));
        mock.body = Some(BodyMatcher::Any);

        let mut context = ctx("POST", "/login");
        context
            .query_params
            .insert("v".to_string(), "2".to_string());
        context
            .headers
            .insert("x-api-key".to_string(), "abc".to_string());
        context.body = Some(Bytes::from("{}"));
        context.content_type = Some("application/json".to_string());

        let result = find_matching_mock(&context, &store(vec![mock]), None).unwrap();
        assert_eq!(result.breakdown.total(), result.score);
        assert_eq!(result.score, 1000 + 100 + 50 + 500);
    }

    // ── the boolean view and the reason view agree ──────────────────────

    #[test]
    fn a_reason_is_produced_exactly_when_the_matcher_rejects() {
        // The guarantee the whole design rests on: `match_*` is defined as
        // `reject_reason().is_none()`, so the dashboard can never describe a
        // rule the server doesn't enforce.
        let mut required = HashMap::new();
        required.insert(
            "x-api-key".to_string(),
            HeaderValue::Exact("secret".to_string()),
        );
        let matcher = HeaderMatcher {
            required,
            forbidden: Vec::new(),
            strict: false,
        };

        let mut accepted = HashMap::new();
        accepted.insert("x-api-key".to_string(), "secret".to_string());
        assert!(match_headers(&accepted, &matcher));
        assert!(headers_reject_reason(&accepted, &matcher).is_none());

        let mut rejected = HashMap::new();
        rejected.insert("x-api-key".to_string(), "wrong".to_string());
        assert!(!match_headers(&rejected, &matcher));
        assert!(headers_reject_reason(&rejected, &matcher).is_some());
    }

    #[test]
    fn evaluate_mock_and_calculate_match_score_agree() {
        let mut mock = plain_mock("GET", "/users");
        mock.headers = Some(header_matcher(
            "x-api-key",
            HeaderValue::Exact("secret".to_string()),
        ));

        let missing = ctx("GET", "/users");
        assert!(matches!(
            evaluate_mock(&missing, &mock),
            Evaluation::Rejected(RejectReason::HeaderMissing { .. })
        ));
        assert!(calculate_match_score(&missing, &mock).is_none());

        let mut present = ctx("GET", "/users");
        present
            .headers
            .insert("x-api-key".to_string(), "secret".to_string());
        assert!(matches!(
            evaluate_mock(&present, &mock),
            Evaluation::Matched(_)
        ));
        assert_eq!(
            calculate_match_score(&present, &mock).map(|b| b.total()),
            Some(1050)
        );
    }
}

// ============================================================================
// Scenario tag filtering (#62)
// ============================================================================

#[cfg(test)]
mod scenario_tests {
    use super::*;
    use crate::types::{BodyMatcher, MockConfig};

    fn mock(method: &str, path: &str, marker: &str, tags: &[&str]) -> MockConfig {
        MockConfig {
            method: method.to_string(),
            path: path.to_string(),
            status: 200,
            response: serde_json::json!({"source": marker}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn store(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for m in mocks {
            let key = crate::types::create_mock_key(&m.method, &m.path);
            map.entry(key).or_default().push(m);
        }
        map
    }

    fn active(tags: &[&str]) -> HashSet<String> {
        tags.iter().map(|t| t.to_string()).collect()
    }

    fn ctx(method: &str, path: &str) -> RequestContext {
        RequestContext::new(method.to_string(), path.to_string())
    }

    #[test]
    fn untagged_mocks_match_regardless_of_the_active_scenario() {
        let mocks = store(vec![mock("GET", "/users", "plain", &[])]);

        for tags in [None, Some(active(&["anything"])), Some(HashSet::new())] {
            let result = find_matching_mock(&ctx("GET", "/users"), &mocks, tags.as_ref())
                .expect("untagged mocks are always matchable");
            assert_eq!(result.mock.response["source"], "plain");
        }
    }

    #[test]
    fn a_tagged_mock_matches_only_while_one_of_its_tags_is_active() {
        let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);

        // No filter at all: tags are inert, exactly as before this feature.
        assert!(find_matching_mock(&ctx("POST", "/checkout"), &mocks, None).is_some());

        let on = active(&["error-scenario"]);
        assert!(find_matching_mock(&ctx("POST", "/checkout"), &mocks, Some(&on)).is_some());

        let off = active(&["happy-path"]);
        assert!(
            find_matching_mock(&ctx("POST", "/checkout"), &mocks, Some(&off)).is_none(),
            "an inactive mock must 404 as if it weren't loaded"
        );
    }

    #[test]
    fn switching_tags_switches_which_mock_serves_a_path() {
        let mocks = store(vec![
            mock("POST", "/checkout", "ok", &["happy-path"]),
            mock("POST", "/checkout", "boom", &["error-scenario"]),
        ]);

        let happy = find_matching_mock(
            &ctx("POST", "/checkout"),
            &mocks,
            Some(&active(&["happy-path"])),
        )
        .expect("happy-path mock is active");
        assert_eq!(happy.mock.response["source"], "ok");
        assert_eq!(happy.index, 0);

        let error = find_matching_mock(
            &ctx("POST", "/checkout"),
            &mocks,
            Some(&active(&["error-scenario"])),
        )
        .expect("error-scenario mock is active");
        assert_eq!(error.mock.response["source"], "boom");
        // The index still points at the mock's real position in its bucket, so
        // sequence counters and hit counts stay keyed to the right mock.
        assert_eq!(error.index, 1);
    }

    #[test]
    fn an_inactive_mock_falls_through_to_its_untagged_sibling() {
        let mocks = store(vec![
            mock("GET", "/users", "default", &[]),
            mock("GET", "/users", "chaos", &["error-scenario"]),
        ]);

        let result = find_matching_mock(
            &ctx("GET", "/users"),
            &mocks,
            Some(&active(&["happy-path"])),
        )
        .expect("the untagged mock is still matchable");
        assert_eq!(result.mock.response["source"], "default");
    }

    #[test]
    fn tag_filtering_applies_to_pattern_routes_too() {
        let mocks = store(vec![mock(
            "GET",
            "/users/:id",
            "pattern",
            &["error-scenario"],
        )]);

        assert!(find_matching_mock(&ctx("GET", "/users/42"), &mocks, None).is_some());
        assert!(find_matching_mock(
            &ctx("GET", "/users/42"),
            &mocks,
            Some(&active(&["error-scenario"]))
        )
        .is_some());
        assert!(find_matching_mock(
            &ctx("GET", "/users/42"),
            &mocks,
            Some(&active(&["happy-path"]))
        )
        .is_none());
    }

    #[test]
    fn an_inactive_exact_mock_falls_through_to_an_active_pattern_route() {
        let mocks = store(vec![
            mock("GET", "/users/42", "exact", &["error-scenario"]),
            mock("GET", "/users/:id", "pattern", &[]),
        ]);

        let result = find_matching_mock(
            &ctx("GET", "/users/42"),
            &mocks,
            Some(&active(&["happy-path"])),
        )
        .expect("the pattern route should serve the request");
        assert_eq!(result.mock.response["source"], "pattern");
        assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
    }

    #[test]
    fn requires_body_ignores_mocks_the_scenario_hides() {
        let mut hungry = mock("POST", "/checkout", "boom", &["error-scenario"]);
        hungry.body = Some(BodyMatcher::Any);
        let mocks = store(vec![mock("POST", "/checkout", "ok", &[]), hungry]);

        // With the tagged mock active, its body matcher forces a body read.
        assert!(requires_body(
            "POST",
            "/checkout",
            &mocks,
            Some(&active(&["error-scenario"]))
        ));

        // Hidden by the scenario, it has no say — the untagged mock left
        // standing doesn't want the body, so `consume_body: false` holds.
        assert!(!requires_body(
            "POST",
            "/checkout",
            &mocks,
            Some(&active(&["happy-path"]))
        ));
    }

    #[test]
    fn requires_body_reads_the_body_when_every_candidate_is_hidden() {
        // Nothing matchable is left, so the request is heading for a 404 and
        // its body is worth capturing for the request log.
        let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);
        assert!(requires_body(
            "POST",
            "/checkout",
            &mocks,
            Some(&active(&["happy-path"]))
        ));
    }

    #[test]
    fn explain_no_match_names_the_scenario_as_the_reason() {
        let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);

        let explanation = explain_no_match(
            &ctx("POST", "/checkout"),
            &mocks,
            Some(&active(&["happy-path"])),
        )
        .expect("a filtered-out mock is worth explaining");

        assert!(
            explanation.contains("1 mock(s) match") && explanation.contains("inactive tags"),
            "unhelpful explanation: {}",
            explanation
        );
    }

    #[test]
    fn explain_no_match_ignores_hidden_mocks_that_would_not_have_matched_anyway() {
        // Right path, wrong method: the tagged mock is hidden *and* irrelevant,
        // so the explanation should stay the "wrong method" one.
        let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);

        let explanation = explain_no_match(
            &ctx("GET", "/checkout"),
            &mocks,
            Some(&active(&["happy-path"])),
        )
        .expect("an unregistered route is always explained");

        assert!(!explanation.contains("inactive tags"), "{}", explanation);
        assert!(explanation.contains("POST"), "{}", explanation);
    }
}
