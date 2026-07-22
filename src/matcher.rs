//! Request matching module for advanced mock matching.
//!
//! This module provides matching logic for:
//! - Query parameters
//! - HTTP headers
//! - Request body (JSON, text, form)

use crate::types::{
    BodyMatcher, FormBodyMatcher, HeaderMatcher, HeaderPattern, HeaderValue, JsonBodyMatcher,
    MockConfig, QueryParamMatcher, QueryParamPattern, QueryParamValue, TextBodyMatcher,
};
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;
use tracing::{debug, warn};

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
pub fn match_query_params(
    request_params: &HashMap<String, String>,
    matcher: &QueryParamMatcher,
) -> bool {
    // Check all required params match
    for (key, expected_value) in &matcher.params {
        match request_params.get(key) {
            Some(actual_value) => {
                if !match_query_param_value(actual_value, expected_value) {
                    debug!(
                        "Query param '{}' mismatch: expected {:?}, got '{}'",
                        key, expected_value, actual_value
                    );
                    return false;
                }
            }
            None => {
                debug!("Required query param '{}' is missing", key);
                return false;
            }
        }
    }

    // If strict mode, check for extra params
    if matcher.strict {
        for key in request_params.keys() {
            if !matcher.params.contains_key(key) {
                debug!("Strict mode: extra query param '{}' found", key);
                return false;
            }
        }
    }

    true
}

/// Match a single query parameter value
fn match_query_param_value(actual: &str, expected: &QueryParamValue) -> bool {
    match expected {
        QueryParamValue::Exact(value) => actual == value,
        QueryParamValue::Pattern(pattern) => match pattern {
            QueryParamPattern::Regex(regex_str) => match Regex::new(regex_str) {
                Ok(re) => re.is_match(actual),
                Err(e) => {
                    warn!("Invalid regex pattern '{}': {}", regex_str, e);
                    false
                }
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

/// Check if request headers match the mock's requirements
pub fn match_headers(request_headers: &HashMap<String, String>, matcher: &HeaderMatcher) -> bool {
    // Check all required headers match
    for (name, expected_value) in &matcher.required {
        let normalized_name = name.to_lowercase();

        match request_headers.get(&normalized_name) {
            Some(actual_value) => {
                if !match_header_value(actual_value, expected_value) {
                    debug!(
                        "Header '{}' mismatch: expected {:?}, got '{}'",
                        name, expected_value, actual_value
                    );
                    return false;
                }
            }
            None => {
                debug!("Required header '{}' is missing", name);
                return false;
            }
        }
    }

    // Check forbidden headers are not present
    for forbidden_name in &matcher.forbidden {
        let normalized_name = forbidden_name.to_lowercase();

        if request_headers.contains_key(&normalized_name) {
            debug!("Forbidden header '{}' is present", forbidden_name);
            return false;
        }
    }

    // If strict mode, check for extra headers (excluding standard ones)
    if matcher.strict {
        const IGNORED_HEADERS: &[&str] = &[
            "host",
            "user-agent",
            "accept-encoding",
            "connection",
            "content-length",
        ];

        for name in request_headers.keys() {
            if IGNORED_HEADERS.contains(&name.as_str()) {
                continue;
            }

            let is_required = matcher
                .required
                .keys()
                .any(|req| req.to_lowercase() == *name);

            if !is_required {
                debug!("Strict mode: extra header '{}' found", name);
                return false;
            }
        }
    }

    true
}

/// Match a single header value
fn match_header_value(actual: &str, expected: &HeaderValue) -> bool {
    match expected {
        HeaderValue::Exact(value) => actual == value,
        HeaderValue::Pattern(pattern) => match pattern {
            HeaderPattern::Regex(regex_str) => match Regex::new(regex_str) {
                Ok(re) => re.is_match(actual),
                Err(e) => {
                    warn!("Invalid regex pattern '{}': {}", regex_str, e);
                    false
                }
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
pub fn match_body(parsed_body: &ParsedBody, matcher: &BodyMatcher) -> bool {
    match matcher {
        BodyMatcher::Any => true,

        BodyMatcher::Empty => matches!(parsed_body, ParsedBody::Empty),

        BodyMatcher::Json(json_matcher) => {
            if let ParsedBody::Json(actual) = parsed_body {
                match_json_body(actual, json_matcher)
            } else if let ParsedBody::Text(text) = parsed_body {
                // Try to parse text as JSON
                if let Ok(actual) = serde_json::from_str(text) {
                    match_json_body(&actual, json_matcher)
                } else {
                    debug!("Body is not valid JSON");
                    false
                }
            } else {
                debug!("Expected JSON body, got {:?}", parsed_body);
                false
            }
        }

        BodyMatcher::Text(text_matcher) => {
            if let ParsedBody::Text(actual) = parsed_body {
                match_text_body(actual, text_matcher)
            } else if let ParsedBody::Json(json) = parsed_body {
                // Convert JSON to string for text matching
                let text = json.to_string();
                match_text_body(&text, text_matcher)
            } else {
                debug!("Expected text body");
                false
            }
        }

        BodyMatcher::Form(form_matcher) => {
            if let ParsedBody::Form(actual) = parsed_body {
                match_form_body(actual, form_matcher)
            } else if let ParsedBody::Text(text) = parsed_body {
                // Try to parse as form data
                let form = parse_form_data(text);
                match_form_body(&form, form_matcher)
            } else {
                debug!("Expected form body");
                false
            }
        }
    }
}

/// Match JSON body
fn match_json_body(actual: &serde_json::Value, matcher: &JsonBodyMatcher) -> bool {
    // Exact match
    if let Some(ref expected) = matcher.exact {
        if actual != expected {
            debug!("JSON exact match failed");
            return false;
        }
    }

    // Partial match
    if let Some(ref expected) = matcher.partial {
        if !match_json_partial(actual, expected, matcher.strict) {
            debug!("JSON partial match failed");
            return false;
        }
    }

    true
}

/// Partial JSON match: check if actual contains all fields from expected
fn match_json_partial(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    strict: bool,
) -> bool {
    match (actual, expected) {
        (serde_json::Value::Object(actual_obj), serde_json::Value::Object(expected_obj)) => {
            // Check all expected fields exist and match
            for (key, expected_value) in expected_obj {
                match actual_obj.get(key) {
                    Some(actual_value) => {
                        if !match_json_partial(actual_value, expected_value, strict) {
                            return false;
                        }
                    }
                    None => {
                        debug!("Expected field '{}' missing in actual", key);
                        return false;
                    }
                }
            }

            // If strict, check for extra fields
            if strict {
                for key in actual_obj.keys() {
                    if !expected_obj.contains_key(key) {
                        debug!("Strict mode: extra field '{}' in actual", key);
                        return false;
                    }
                }
            }

            true
        }

        (serde_json::Value::Array(actual_arr), serde_json::Value::Array(expected_arr)) => {
            if strict && actual_arr.len() != expected_arr.len() {
                return false;
            }

            // Check if all expected items match in order
            for (i, expected_item) in expected_arr.iter().enumerate() {
                if let Some(actual_item) = actual_arr.get(i) {
                    if !match_json_partial(actual_item, expected_item, strict) {
                        return false;
                    }
                } else {
                    return false;
                }
            }

            true
        }

        _ => actual == expected,
    }
}

/// Match text body
fn match_text_body(actual: &str, matcher: &TextBodyMatcher) -> bool {
    // Exact match
    if let Some(ref expected) = matcher.exact {
        if actual != expected {
            debug!("Text exact match failed");
            return false;
        }
    }

    // Contains
    if let Some(ref substring) = matcher.contains {
        if !actual.contains(substring) {
            debug!("Text does not contain '{}'", substring);
            return false;
        }
    }

    // Regex
    if let Some(ref pattern) = matcher.regex {
        match Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(actual) {
                    debug!("Text does not match regex '{}'", pattern);
                    return false;
                }
            }
            Err(e) => {
                warn!("Invalid regex pattern '{}': {}", pattern, e);
                return false;
            }
        }
    }

    true
}

/// Match form body
fn match_form_body(actual: &HashMap<String, String>, matcher: &FormBodyMatcher) -> bool {
    // Check all required fields match
    for (key, expected_value) in &matcher.fields {
        match actual.get(key) {
            Some(actual_value) => {
                if actual_value != expected_value {
                    debug!(
                        "Form field '{}' mismatch: expected '{}', got '{}'",
                        key, expected_value, actual_value
                    );
                    return false;
                }
            }
            None => {
                debug!("Required form field '{}' is missing", key);
                return false;
            }
        }
    }

    // If strict, check for extra fields
    if matcher.strict {
        for key in actual.keys() {
            if !matcher.fields.contains_key(key) {
                debug!("Strict mode: extra form field '{}' found", key);
                return false;
            }
        }
    }

    true
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

/// Compile a path template into a regex with one (unnamed, positional)
/// capture group per parameter segment — using positional groups rather
/// than Rust regex's named-group syntax means parameter names aren't
/// restricted to `[A-Za-z0-9_]` (e.g. `:org-id` works).
///
/// Returns `None` if the path has no parameter segments, or the resulting
/// regex fails to compile.
pub fn compile_path_pattern(path: &str) -> Option<CompiledPathPattern> {
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
}

/// Find the best matching mock for a request.
///
/// Exact "METHOD:path" lookups are O(1) and tried first; parameterized
/// paths (`:id`, `{id}`) are only checked afterward, by scanning mocks
/// registered under the same method, and always score lower — so an exact
/// path match wins over a pattern match whenever both are eligible.
pub fn find_matching_mock(
    context: &RequestContext,
    mocks: &HashMap<String, Vec<MockConfig>>,
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
            if let Some(score) = calculate_match_score(context, mock) {
                candidates.push(MatchResult {
                    mock: mock.clone(),
                    score,
                    index,
                    path_params: HashMap::new(),
                    matched_key: base_key.clone(),
                });
            }
        }
    }

    // Pattern match: scan mocks registered under the same method whose path
    // uses :name/{name} segments.
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
            if let Some(score) = calculate_match_score(context, mock) {
                candidates.push(MatchResult {
                    mock: mock.clone(),
                    score: score.saturating_sub(PATTERN_MATCH_PENALTY),
                    index,
                    path_params: params.clone(),
                    matched_key: key.clone(),
                });
            }
        }
    }

    // Sort by score (highest first) and return best match
    candidates.sort_by_key(|b| std::cmp::Reverse(b.score));

    if let Some(best) = candidates.into_iter().next() {
        debug!("Found matching mock with score {}", best.score);
        Some(best)
    } else {
        debug!("No matching mock found");
        None
    }
}

/// Calculate match score for a mock
/// Returns None if the mock doesn't match, Some(score) if it matches
fn calculate_match_score(context: &RequestContext, mock: &MockConfig) -> Option<u32> {
    let mut score: u32 = 1000; // Base score for method + path match

    // Check query params
    if let Some(ref query_matcher) = mock.query_params {
        if !match_query_params(&context.query_params, query_matcher) {
            return None;
        }
        // Add score based on number of matched params
        score += (query_matcher.params.len() as u32) * 100;
    }

    // Check headers
    if let Some(ref header_matcher) = mock.headers {
        if !match_headers(&context.headers, header_matcher) {
            return None;
        }
        // Add score based on number of matched headers
        score += (header_matcher.required.len() as u32) * 50;
    }

    // Check body
    if let Some(ref body_matcher) = mock.body {
        if let Some(ref body_bytes) = context.body {
            let parsed_body = parse_body(body_bytes, context.content_type.as_deref());
            if !match_body(&parsed_body, body_matcher) {
                return None;
            }
            // Body matching is worth the most
            score += 500;
        } else if !matches!(body_matcher, BodyMatcher::Empty | BodyMatcher::Any) {
            // Body required but not present
            return None;
        }
    }

    Some(score)
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
            sequence: None,
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

        let result = find_matching_mock(&make_context("admin"), &mocks).unwrap();
        assert_eq!(result.index, 0);
        assert_eq!(result.mock.response["role"], "admin");
        assert!(result.path_params.is_empty());
        assert_eq!(result.matched_key, "POST:/login");

        let result = find_matching_mock(&make_context("user"), &mocks).unwrap();
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
            sequence: None,
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

        let result = find_matching_mock(&path_param_context("/users/42"), &mocks).unwrap();
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

        let result = find_matching_mock(&context, &mocks).unwrap();
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

        let result =
            find_matching_mock(&path_param_context("/orders/ORD-9981/items/2"), &mocks).unwrap();
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

        let result = find_matching_mock(&path_param_context("/users/42"), &mocks).unwrap();
        assert_eq!(result.mock.response["source"], "exact");
        assert!(result.path_params.is_empty());

        // A different id has no exact mock, so the pattern still matches
        let result = find_matching_mock(&path_param_context("/users/7"), &mocks).unwrap();
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

        assert!(find_matching_mock(&path_param_context("/posts/1"), &mocks).is_none());
    }
}
