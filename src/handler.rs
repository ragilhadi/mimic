use crate::matcher::{
    find_matching_mock, parse_body, parse_headers, parse_query_string, MatchResult, RequestContext,
};
use crate::template::{render_response, TemplateContext};
use crate::types::{MockStore, RequestLog, RequestRecord, SequenceCounters, SequenceStep};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, Method, StatusCode, Uri},
    response::{Html, IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct AppState {
    pub mocks: MockStore,
    pub request_log: RequestLog,
    pub request_counter: Arc<AtomicU64>,
    pub sequence_counters: SequenceCounters,
}

impl AppState {
    pub fn new(mocks: MockStore) -> Self {
        Self {
            mocks,
            request_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            request_counter: Arc::new(AtomicU64::new(0)),
            sequence_counters: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }
}

/// Maximum body size to consume for matching (10 MB)
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

pub async fn handle_request(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    State(state): State<AppState>,
    body: Body,
) -> Response {
    let path = uri.path().to_string();
    let method_str = method.as_str().to_string();

    debug!("Incoming request: {} {}", method_str, uri);

    // Parse query parameters from URI
    let query_params = parse_query_string(uri.query());

    // Parse headers into HashMap (normalized to lowercase)
    let parsed_headers = parse_headers(&headers);

    // Get content type for body parsing
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Check if any mock needs body matching (acquire read lock)
    let needs_body_matching = {
        let mocks = state.mocks.read().await;
        mocks.values().flatten().any(|mock| mock.body.is_some())
    };

    // Consume body if needed for matching or if consume_body is set
    let body_bytes: Option<Bytes> =
        if needs_body_matching || matches!(method, Method::POST | Method::PUT | Method::PATCH) {
            match body.collect().await {
                Ok(collected) => {
                    let bytes = collected.to_bytes();
                    if bytes.len() > MAX_BODY_SIZE {
                        debug!(
                            "Body too large: {} bytes (max: {})",
                            bytes.len(),
                            MAX_BODY_SIZE
                        );
                        None
                    } else {
                        debug!("Consumed {} bytes from request body", bytes.len());
                        Some(bytes)
                    }
                }
                Err(e) => {
                    debug!("Failed to read body: {}", e);
                    None
                }
            }
        } else {
            None
        };

    // Build request context for matching
    let mut context = RequestContext {
        method: method_str.clone(),
        path: path.clone(),
        path_params: HashMap::new(),
        query_params,
        headers: parsed_headers.clone(),
        body: body_bytes,
        content_type,
    };

    // Find matching mock using the matcher (acquire read lock)
    let mocks = state.mocks.read().await;
    let matched = find_matching_mock(&context, &mocks);
    // Release read lock before any await (counter lock, recording, delay)
    drop(mocks);

    match matched {
        Some(MatchResult {
            mock,
            index,
            path_params,
            matched_key: mock_key,
            ..
        }) => {
            // Named path parameters captured from the mock's pattern (e.g.
            // `/users/:id`), if any, become available to templating below.
            context.path_params = path_params;

            // Resolve the response: sequence step if configured, top-level otherwise.
            // An empty sequence array falls back to the top-level status/response.
            // The counter is keyed by the mock's declared path (`mock_key`), not the
            // concrete request path, so a pattern mock like `/users/:id` advances a
            // single shared sequence regardless of which id was requested.
            let (status_u16, response, delay_ms) = match mock.sequence.as_deref() {
                Some(steps) if !steps.is_empty() => {
                    let counter_key = format!("{}#{}", mock_key, index);
                    advance_sequence(&state.sequence_counters, &counter_key, steps).await
                }
                _ => (mock.status, mock.response.clone(), None),
            };

            info!("Mock matched: {} {} -> {}", method_str, path, status_u16);

            // Interpolate {{path.X}}, {{query.X}}, {{header.X}}, {{body.X}} in the
            // response using data from the matched request, before it's consumed
            // by recording below.
            let parsed_body = context
                .body
                .as_ref()
                .map(|b| parse_body(b, context.content_type.as_deref()));
            let template_ctx = TemplateContext {
                path_params: &context.path_params,
                query_params: &context.query_params,
                headers: &context.headers,
                body: parsed_body.as_ref(),
            };
            let response = render_response(&response, &template_ctx);

            let status = StatusCode::from_u16(status_u16).unwrap_or(StatusCode::OK);
            let matched_key = format!("{}:{}", method_str, path);

            // Record the request with the status actually served
            record_request(&state, context, Some(matched_key), status_u16).await;

            // Resolve the delay: a sequence step's own delay wins, otherwise the
            // mock-level delay_ms (fixed or sampled from a range) applies
            let effective_delay = delay_ms.or_else(|| mock.delay_ms.as_ref().map(|d| d.resolve()));

            // Apply the delay last, with no locks held
            if let Some(ms) = effective_delay {
                if ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
            }

            // Return configured response with any custom headers
            build_response(status, &response, mock.response_headers.as_ref())
        }
        None => {
            info!("No mock found for: {} {}", method_str, path);

            // Record the request (clone query_params for use in error response)
            let query_params_clone = context.query_params.clone();
            record_request(&state, context, None, 404).await;

            // Return 404 with detailed error message
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "mock not found",
                    "method": method_str,
                    "path": path,
                    "query_params": query_params_clone,
                    "headers_received": parsed_headers.keys().collect::<Vec<_>>()
                })),
            )
                .into_response()
        }
    }
}

pub async fn health_check(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mocks = state.mocks.read().await;
    Json(json!({
        "status": "healthy",
        "mocks_loaded": mocks.len(),
        "service": "mimic"
    }))
}

/// Headers whose values should be redacted in recorded requests
const SENSITIVE_HEADERS: &[&str] = &["authorization", "cookie", "set-cookie"];

/// Record a request into the request log, redacting sensitive headers
async fn record_request(
    state: &AppState,
    context: RequestContext,
    matched_mock: Option<String>,
    response_status: u16,
) {
    let redacted_headers = context
        .headers
        .into_iter()
        .map(|(k, v)| {
            if SENSITIVE_HEADERS.contains(&k.as_str()) {
                (k, "[REDACTED]".to_string())
            } else {
                (k, v)
            }
        })
        .collect();

    let mut record = RequestRecord {
        id: 0,
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        method: context.method,
        path: context.path,
        query_params: context.query_params,
        headers: redacted_headers,
        body: context
            .body
            .and_then(|b| String::from_utf8(b.to_vec()).ok()),
        matched_mock,
        response_status,
    };
    // IDs are unique but may not be strictly sequential under concurrent load
    let mut log = state.request_log.write().await;
    record.id = state.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
    log.push(record);
}

#[derive(Deserialize, Default)]
pub struct RequestFilter {
    pub path: Option<String>,
    pub method: Option<String>,
    pub status: Option<u16>,
}

pub async fn list_requests(
    State(state): State<AppState>,
    Query(filter): Query<RequestFilter>,
) -> Json<serde_json::Value> {
    let log = state.request_log.read().await;
    let filtered: Vec<&RequestRecord> = log
        .iter()
        .filter(|r| {
            if let Some(ref p) = filter.path {
                if &r.path != p {
                    return false;
                }
            }
            if let Some(ref m) = filter.method {
                if !r.method.eq_ignore_ascii_case(m) {
                    return false;
                }
            }
            if let Some(s) = filter.status {
                if r.response_status != s {
                    return false;
                }
            }
            true
        })
        .collect();

    Json(json!({
        "count": filtered.len(),
        "requests": filtered
    }))
}

pub async fn clear_requests(State(state): State<AppState>) -> StatusCode {
    let mut log = state.request_log.write().await;
    log.clear();
    StatusCode::NO_CONTENT
}

/// Build the mock response: status, custom headers, and body.
///
/// Custom header names are case-insensitive; invalid names/values are skipped
/// with a warning. `Content-Type: application/json` is added only when the
/// custom headers don't already set a content type. When a non-JSON content
/// type is configured and the response value is a JSON string, the raw string
/// is sent as the body (so XML/CSV/plain-text mocks aren't JSON-quoted).
fn build_response(
    status: StatusCode,
    response: &serde_json::Value,
    custom_headers: Option<&HashMap<String, String>>,
) -> Response {
    let mut header_map = HeaderMap::new();
    if let Some(custom) = custom_headers {
        for (name, value) in custom {
            let parsed_name = axum::http::HeaderName::from_bytes(name.as_bytes());
            let parsed_value = axum::http::HeaderValue::from_str(value);
            match (parsed_name, parsed_value) {
                (Ok(n), Ok(v)) => {
                    header_map.insert(n, v);
                }
                _ => warn!("Skipping invalid response header: {}", name),
            }
        }
    }
    if !header_map.contains_key(CONTENT_TYPE) {
        header_map.insert(
            CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
    }

    let is_json_content_type = header_map
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().contains("json"));

    let body = match response {
        serde_json::Value::String(s) if !is_json_content_type => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };

    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    *res.headers_mut() = header_map;
    res
}

/// Pick the current sequence step and advance the counter.
/// The write lock is held only for the map lookup + clone, never across an await.
async fn advance_sequence(
    counters: &SequenceCounters,
    key: &str,
    steps: &[SequenceStep],
) -> (u16, serde_json::Value, Option<u64>) {
    let mut map = counters.write().await;
    let count = map.entry(key.to_string()).or_insert(0);
    // Clamp so the last step keeps repeating once the sequence is exhausted
    let idx = (*count).min(steps.len() - 1);
    let step = &steps[idx];
    if !step.repeat {
        *count += 1;
    }
    (step.status, step.response.clone(), step.delay_ms)
}

#[derive(Deserialize, Default)]
pub struct SequenceResetFilter {
    pub path: Option<String>,
}

pub async fn reset_sequences(
    State(state): State<AppState>,
    Query(filter): Query<SequenceResetFilter>,
) -> Json<serde_json::Value> {
    let mut counters = state.sequence_counters.write().await;
    let removed = match filter.path {
        Some(ref p) => {
            let before = counters.len();
            // Counter keys look like "METHOD:/path#idx" — compare the path part only
            counters.retain(|key, _| {
                let after_method = key.split_once(':').map_or("", |(_, rest)| rest);
                let path_part = after_method
                    .rsplit_once('#')
                    .map_or(after_method, |(path_part, _)| path_part);
                path_part != p
            });
            before - counters.len()
        }
        None => {
            let n = counters.len();
            counters.clear();
            n
        }
    };
    info!("Reset {} sequence counter(s)", removed);
    Json(json!({ "reset": removed }))
}

pub async fn admin_dashboard() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BodyMatcher, DelayConfig, HeaderMatcher, HeaderPattern, HeaderValue, JsonBodyMatcher,
        MockConfig, QueryParamMatcher, QueryParamPattern, QueryParamValue,
    };
    use axum::http::Method;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn create_test_state() -> AppState {
        let mut mocks = HashMap::new();

        mocks.insert(
            "GET:/users".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users".to_string(),
                status: 200,
                response: json!({"users": [{"id": 1, "name": "Alice"}]}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        mocks.insert(
            "POST:/login".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/login".to_string(),
                status: 201,
                response: json!({"token": "test-token"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        mocks.insert(
            "DELETE:/users/123".to_string(),
            vec![MockConfig {
                method: "DELETE".to_string(),
                path: "/users/123".to_string(),
                status: 204,
                response: json!(null),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    fn create_empty_state() -> AppState {
        AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())))
    }

    #[tokio::test]
    async fn test_handle_request_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_request_not_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/nonexistent".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_post_method() {
        let state = create_test_state();
        let method = Method::POST;
        let uri = "/login".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_handle_request_delete_method() {
        let state = create_test_state();
        let method = Method::DELETE;
        let uri = "/users/123".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_handle_request_wrong_method() {
        let state = create_test_state();
        let method = Method::PUT;
        let uri = "/users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_case_sensitive_path() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/Users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = create_test_state();
        let response = health_check(State(state)).await;

        assert_eq!(response.0["status"], "healthy");
        assert_eq!(response.0["mocks_loaded"], 3);
        assert_eq!(response.0["service"], "mimic");
    }

    #[tokio::test]
    async fn test_health_check_empty_state() {
        let state = create_empty_state();
        let response = health_check(State(state)).await;

        assert_eq!(response.0["status"], "healthy");
        assert_eq!(response.0["mocks_loaded"], 0);
    }

    #[tokio::test]
    async fn test_handle_request_response_body() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

        assert_eq!(parts.status, StatusCode::OK);
        assert_eq!(json["users"][0]["name"], "Alice");
    }

    #[tokio::test]
    async fn test_handle_request_not_found_response_body() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/nonexistent".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

        assert_eq!(parts.status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "mock not found");
        assert_eq!(json["method"], "GET");
        assert_eq!(json["path"], "/nonexistent");
    }

    // =========================================================================
    // Query Parameter Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_query_param_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/search".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/search".to_string(),
                status: 200,
                response: json!({"results": []}),
                consume_body: false,
                query_params: Some(QueryParamMatcher {
                    params: HashMap::from([(
                        "q".to_string(),
                        QueryParamValue::Exact("test".to_string()),
                    )]),
                    strict: false,
                }),
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with correct query param
        let method = Method::GET;
        let uri = "/search?q=test".parse().unwrap();
        let headers = HeaderMap::new();
        let response =
            handle_request(method, uri, headers, State(state.clone()), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with wrong query param
        let uri = "/search?q=wrong".parse().unwrap();
        let headers = HeaderMap::new();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_query_param_regex_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users".to_string(),
                status: 200,
                response: json!({"users": []}),
                consume_body: false,
                query_params: Some(QueryParamMatcher {
                    params: HashMap::from([(
                        "page".to_string(),
                        QueryParamValue::Pattern(QueryParamPattern::Regex("^[0-9]+$".to_string())),
                    )]),
                    strict: false,
                }),
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with numeric page
        let uri = "/users?page=123".parse().unwrap();
        let headers = HeaderMap::new();
        let response = handle_request(
            Method::GET,
            uri,
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with non-numeric page
        let uri = "/users?page=abc".parse().unwrap();
        let headers = HeaderMap::new();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Header Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_header_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/protected".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/protected".to_string(),
                status: 200,
                response: json!({"data": "secret"}),
                consume_body: false,
                query_params: None,
                headers: Some(HeaderMatcher {
                    required: HashMap::from([(
                        "authorization".to_string(),
                        HeaderValue::Exact("Bearer token123".to_string()),
                    )]),
                    forbidden: vec![],
                    strict: false,
                }),
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with correct header
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token123".parse().unwrap());
        let uri = "/protected".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with wrong header
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        let uri = "/protected".parse().unwrap();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_header_prefix_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/api".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/api".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: Some(HeaderMatcher {
                    required: HashMap::from([(
                        "authorization".to_string(),
                        HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
                    )]),
                    forbidden: vec![],
                    strict: false,
                }),
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with any Bearer token
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer anytoken".parse().unwrap());
        let uri = "/api".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with Basic auth
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic abc123".parse().unwrap());
        let uri = "/api".parse().unwrap();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Body Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_json_body_exact_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/login".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/login".to_string(),
                status: 200,
                response: json!({"token": "abc123"}),
                consume_body: true,
                query_params: None,
                headers: None,
                body: Some(BodyMatcher::Json(JsonBodyMatcher {
                    exact: Some(json!({"username": "admin", "password": "secret"})),
                    partial: None,
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with exact body
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"username":"admin","password":"secret"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with different body
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"username":"admin","password":"wrong"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_json_body_partial_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/users".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/users".to_string(),
                status: 201,
                response: json!({"id": 1}),
                consume_body: true,
                query_params: None,
                headers: None,
                body: Some(BodyMatcher::Json(JsonBodyMatcher {
                    exact: None,
                    partial: Some(json!({"name": "Alice"})),
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with partial body (extra fields ignored)
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/users".parse().unwrap();
        let body = Body::from(r#"{"name":"Alice","email":"alice@example.com"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::CREATED);

        // Should not match with wrong name
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/users".parse().unwrap();
        let body = Body::from(r#"{"name":"Bob"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Combined Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_combined_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/api/search".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/api/search".to_string(),
                status: 200,
                response: json!({"results": ["item1"]}),
                consume_body: true,
                query_params: Some(QueryParamMatcher {
                    params: HashMap::from([(
                        "type".to_string(),
                        QueryParamValue::Exact("user".to_string()),
                    )]),
                    strict: false,
                }),
                headers: Some(HeaderMatcher {
                    required: HashMap::from([(
                        "authorization".to_string(),
                        HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
                    )]),
                    forbidden: vec![],
                    strict: false,
                }),
                body: Some(BodyMatcher::Json(JsonBodyMatcher {
                    exact: None,
                    partial: Some(json!({"query": "Alice"})),
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with all criteria met
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/api/search?type=user".parse().unwrap();
        let body = Body::from(r#"{"query":"Alice"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with wrong query param
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/api/search?type=product".parse().unwrap();
        let body = Body::from(r#"{"query":"Alice"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Response Templating Tests
    // =========================================================================

    #[tokio::test]
    async fn test_template_interpolates_body_query_and_header() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/users".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/users".to_string(),
                status: 201,
                response: json!({
                    "id": 99,
                    "username": "{{body.username}}",
                    "email": "{{body.email}}",
                    "created_by": "{{header.x-actor}}",
                    "welcomed_on_page": "{{query.page}}"
                }),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("x-actor", "admin".parse().unwrap());
        let uri = "/users?page=2".parse().unwrap();
        let body = Body::from(r#"{"username":"alice","email":"alice@example.com"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;

        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 99);
        assert_eq!(json["username"], "alice");
        assert_eq!(json["email"], "alice@example.com");
        assert_eq!(json["created_by"], "admin");
        assert_eq!(json["welcomed_on_page"], "2");
    }

    #[tokio::test]
    async fn test_template_nested_body_field() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/orders".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/orders".to_string(),
                status: 200,
                response: json!({"shipping_city": "{{body.address.city}}"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/orders".parse().unwrap();
        let body = Body::from(r#"{"address":{"city":"Jakarta"}}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["shipping_city"], "Jakarta");
    }

    #[tokio::test]
    async fn test_template_missing_variable_renders_empty_string() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/profile".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/profile".to_string(),
                status: 200,
                response: json!({"nickname": "{{query.nickname}}"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let headers = HeaderMap::new();
        let uri = "/profile".parse().unwrap();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["nickname"], "");
    }

    #[tokio::test]
    async fn test_response_without_templates_is_unaffected() {
        let state = create_test_state();
        let uri = "/users".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json, json!({"users": [{"id": 1, "name": "Alice"}]}));
    }

    #[tokio::test]
    async fn test_template_in_sequence_step_response() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/echo".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/echo".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: Some(vec![SequenceStep {
                    status: 200,
                    response: json!({"echoed": "{{body.message}}"}),
                    delay_ms: None,
                    repeat: true,
                }]),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/echo".parse().unwrap();
        let body = Body::from(r#"{"message":"hello"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["echoed"], "hello");
    }

    // =========================================================================
    // Path Parameter Tests
    // =========================================================================

    #[tokio::test]
    async fn test_path_param_matches_and_templates_into_response() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/:id".to_string(),
                status: 200,
                response: json!({"id": "{{path.id}}", "name": "Mock User"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
        let uri = "/users/42".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], "42");
        assert_eq!(json["name"], "Mock User");
    }

    #[tokio::test]
    async fn test_path_param_brace_syntax_multiple_params() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "DELETE:/orgs/{org}/repos/{repo}".to_string(),
            vec![MockConfig {
                method: "DELETE".to_string(),
                path: "/orgs/{org}/repos/{repo}".to_string(),
                status: 204,
                response: json!(null),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
        let uri = "/orgs/acme/repos/widgets".parse().unwrap();
        let response = handle_request(
            Method::DELETE,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_exact_path_wins_over_path_param_pattern() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/:id".to_string(),
                status: 200,
                response: json!({"source": "pattern"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );
        mocks.insert(
            "GET:/users/42".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/42".to_string(),
                status: 200,
                response: json!({"source": "exact"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // The exact mock wins for id=42
        let uri = "/users/42".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["source"], "exact");

        // Any other id falls through to the pattern mock
        let uri = "/users/7".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["source"], "pattern");
    }

    #[tokio::test]
    async fn test_path_param_sequence_shared_across_ids() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/items/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/items/:id".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: Some(vec![
                    SequenceStep {
                        status: 503,
                        response: json!({"error": "unavailable"}),
                        delay_ms: None,
                        repeat: false,
                    },
                    SequenceStep {
                        status: 200,
                        response: json!({"ok": true}),
                        delay_ms: None,
                        repeat: true,
                    },
                ]),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // First request for id=1 consumes step 0 (503)
        let uri = "/items/1".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // A request for a *different* id shares the same sequence counter
        // (keyed by the mock's declared pattern, not the concrete path), so
        // it now sees step 1 (200), not step 0 again.
        let uri = "/items/2".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_no_path_param_match_returns_404() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/:id".to_string(),
                status: 200,
                response: json!({"name": "Mock User"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                sequence: None,
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
        let uri = "/posts/1".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Multiple Mocks per METHOD:PATH Tests
    // =========================================================================

    #[tokio::test]
    async fn test_multiple_mocks_same_path_body_differentiation() {
        // Reproduce the bug: two POST /login mocks differ only by body matcher.
        // Both must be retained and the highest-scoring one must win.
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/login".to_string(),
            vec![
                MockConfig {
                    method: "POST".to_string(),
                    path: "/login".to_string(),
                    status: 200,
                    response: json!({"role": "admin"}),
                    consume_body: true,
                    query_params: None,
                    headers: None,
                    body: Some(BodyMatcher::Json(JsonBodyMatcher {
                        exact: None,
                        partial: Some(json!({"role": "admin"})),
                        strict: false,
                    })),
                    delay_ms: None,
                    response_headers: None,
                    sequence: None,
                },
                MockConfig {
                    method: "POST".to_string(),
                    path: "/login".to_string(),
                    status: 200,
                    response: json!({"role": "user"}),
                    consume_body: true,
                    query_params: None,
                    headers: None,
                    body: Some(BodyMatcher::Json(JsonBodyMatcher {
                        exact: None,
                        partial: Some(json!({"role": "user"})),
                        strict: false,
                    })),
                    delay_ms: None,
                    response_headers: None,
                    sequence: None,
                },
            ],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Request with admin role should return admin mock
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"role":"admin"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (_, resp_body) = response.into_parts();
        let bytes = resp_body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["role"], "admin");

        // Request with user role should return user mock
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"role":"user"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (_, resp_body) = response.into_parts();
        let bytes = resp_body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["role"], "user");
    }

    // =========================================================================
    // Request History API Tests
    // =========================================================================

    #[tokio::test]
    async fn test_list_requests_empty() {
        let state = create_empty_state();
        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 0);
        assert_eq!(response.0["requests"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_request_recording() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        let requests = response.0["requests"].as_array().unwrap();
        assert_eq!(requests[0]["id"], 1);
        assert_eq!(requests[0]["method"], "GET");
        assert_eq!(requests[0]["path"], "/users");
        assert_eq!(requests[0]["response_status"], 200);
        assert_eq!(requests[0]["matched_mock"], "GET:/users");
    }

    #[tokio::test]
    async fn test_request_recording_not_found() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/nonexistent".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        let requests = response.0["requests"].as_array().unwrap();
        assert_eq!(requests[0]["response_status"], 404);
        assert!(requests[0]["matched_mock"].is_null());
    }

    #[tokio::test]
    async fn test_request_filtering_by_path() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter {
            path: Some("/users".to_string()),
            method: None,
            status: None,
        });
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        assert_eq!(response.0["requests"][0]["path"], "/users");
    }

    #[tokio::test]
    async fn test_request_filtering_by_method() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter {
            path: None,
            method: Some("POST".to_string()),
            status: None,
        });
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        assert_eq!(response.0["requests"][0]["method"], "POST");
    }

    #[tokio::test]
    async fn test_request_filtering_by_status() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::GET,
            "/missing".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter {
            path: None,
            method: None,
            status: Some(404),
        });
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        assert_eq!(response.0["requests"][0]["response_status"], 404);
    }

    #[tokio::test]
    async fn test_clear_requests() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        {
            let log = state.request_log.read().await;
            assert_eq!(log.len(), 1);
        }

        let status = clear_requests(State(state.clone())).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 0);
    }

    #[tokio::test]
    async fn test_request_ids_increment() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 2);
        assert_eq!(response.0["requests"][0]["id"], 1);
        assert_eq!(response.0["requests"][1]["id"], 2);
    }

    #[tokio::test]
    async fn test_sensitive_headers_redacted() {
        let state = create_test_state();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret-token".parse().unwrap());
        headers.insert("cookie", "session=abc123".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        let requests = response.0["requests"].as_array().unwrap();
        let recorded_headers = requests[0]["headers"].as_object().unwrap();
        assert_eq!(recorded_headers["authorization"], "[REDACTED]");
        assert_eq!(recorded_headers["cookie"], "[REDACTED]");
        assert_eq!(recorded_headers["content-type"], "application/json");
    }

    // =========================================================================
    // Stateful Sequence Tests
    // =========================================================================

    fn sequence_mock(path: &str, steps: Vec<SequenceStep>) -> MockConfig {
        MockConfig {
            method: "GET".to_string(),
            path: path.to_string(),
            status: 200,
            response: json!({"fallback": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            sequence: Some(steps),
        }
    }

    fn step(status: u16, body: serde_json::Value) -> SequenceStep {
        SequenceStep {
            status,
            response: body,
            delay_ms: None,
            repeat: false,
        }
    }

    fn sequence_state(path: &str, steps: Vec<SequenceStep>) -> AppState {
        let mocks = HashMap::from([(format!("GET:{}", path), vec![sequence_mock(path, steps)])]);
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    async fn call(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
        let response = handle_request(
            Method::GET,
            path.parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (parts.status, json)
    }

    #[tokio::test]
    async fn test_sequence_steps_consumed_in_order() {
        let state = sequence_state(
            "/seq",
            vec![
                step(200, json!({"n": 1})),
                step(429, json!({"n": 2})),
                step(500, json!({"n": 3})),
            ],
        );

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["n"], 1);

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["n"], 2);

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["n"], 3);
    }

    #[tokio::test]
    async fn test_sequence_exhaustion_last_step_repeats() {
        let state = sequence_state(
            "/seq",
            vec![step(201, json!({"n": 1})), step(500, json!({"n": 2}))],
        );

        let _ = call(&state, "/seq").await;
        let _ = call(&state, "/seq").await;

        // Sequence exhausted: last step keeps repeating even without repeat: true
        for _ in 0..3 {
            let (status, body) = call(&state, "/seq").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body["n"], 2);
        }
    }

    #[tokio::test]
    async fn test_sequence_repeat_step_sticks() {
        let mut repeat_step = step(200, json!({"ok": true}));
        repeat_step.repeat = true;
        let state = sequence_state(
            "/seq",
            vec![
                step(503, json!({"error": "unavailable"})),
                repeat_step,
                step(500, json!({"never": true})),
            ],
        );

        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        // The repeat step pins the counter; the step after it is never served
        for _ in 0..3 {
            let (status, body) = call(&state, "/seq").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["ok"], true);
        }
    }

    #[tokio::test]
    async fn test_empty_sequence_falls_back_to_top_level() {
        let state = sequence_state("/seq", vec![]);

        for _ in 0..2 {
            let (status, body) = call(&state, "/seq").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["fallback"], true);
        }
    }

    #[tokio::test]
    async fn test_sequence_delay_applied() {
        let mut delayed = step(200, json!({"ok": true}));
        delayed.delay_ms = Some(30);
        let state = sequence_state("/seq", vec![delayed]);

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "expected at least 30ms delay, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_sequence_invalid_status_falls_back_to_ok() {
        // 99 is below the valid HTTP status range; from_u16 rejects it
        let state = sequence_state("/seq", vec![step(99, json!({"weird": true}))]);

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["weird"], true);
    }

    #[tokio::test]
    async fn test_sequence_counters_independent_per_variant() {
        // Two sequenced mocks sharing POST:/login, split by body matcher.
        // Each must advance its own counter (proves the #index key).
        let make_mock = |role: &str, steps: Vec<SequenceStep>| MockConfig {
            method: "POST".to_string(),
            path: "/login".to_string(),
            status: 200,
            response: json!({"fallback": true}),
            consume_body: true,
            query_params: None,
            headers: None,
            body: Some(BodyMatcher::Json(JsonBodyMatcher {
                exact: None,
                partial: Some(json!({"role": role})),
                strict: false,
            })),
            delay_ms: None,
            response_headers: None,
            sequence: Some(steps),
        };
        let mocks = HashMap::from([(
            "POST:/login".to_string(),
            vec![
                make_mock(
                    "admin",
                    vec![step(200, json!({"n": 1})), step(201, json!({"n": 2}))],
                ),
                make_mock(
                    "user",
                    vec![step(202, json!({"n": 1})), step(203, json!({"n": 2}))],
                ),
            ],
        )]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let post = |state: AppState, role: &'static str| async move {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "application/json".parse().unwrap());
            let body = Body::from(format!(r#"{{"role":"{}"}}"#, role));
            let response = handle_request(
                Method::POST,
                "/login".parse().unwrap(),
                headers,
                State(state),
                body,
            )
            .await;
            response.status()
        };

        // Interleave: each variant advances independently
        assert_eq!(post(state.clone(), "admin").await, StatusCode::OK);
        assert_eq!(post(state.clone(), "user").await, StatusCode::ACCEPTED);
        assert_eq!(post(state.clone(), "admin").await, StatusCode::CREATED);
        let status = post(state.clone(), "user").await;
        assert_eq!(status, StatusCode::NON_AUTHORITATIVE_INFORMATION);
    }

    #[tokio::test]
    async fn test_reset_sequences_all() {
        let state = sequence_state(
            "/seq",
            vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))],
        );

        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);

        let filter = SequenceResetFilter::default();
        let response = reset_sequences(State(state.clone()), Query(filter)).await;
        assert_eq!(response.0["reset"], 1);

        // Sequence starts over from step 1
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_reset_sequences_by_path() {
        let mocks = HashMap::from([
            (
                "GET:/a".to_string(),
                vec![sequence_mock(
                    "/a",
                    vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))],
                )],
            ),
            (
                "GET:/b".to_string(),
                vec![sequence_mock(
                    "/b",
                    vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))],
                )],
            ),
        ]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let _ = call(&state, "/a").await;
        let _ = call(&state, "/b").await;

        let filter = SequenceResetFilter {
            path: Some("/a".to_string()),
        };
        let response = reset_sequences(State(state.clone()), Query(filter)).await;
        assert_eq!(response.0["reset"], 1);

        // /a restarts from step 1, /b continues from step 2
        let (status, _) = call(&state, "/a").await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(&state, "/b").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sequence_thread_safety() {
        let mut repeat_step = step(200, json!({"ok": true}));
        repeat_step.repeat = true;
        let state = sequence_state(
            "/seq",
            vec![
                step(201, json!({"n": 1})),
                step(202, json!({"n": 2})),
                repeat_step,
            ],
        );

        let mut handles = Vec::new();
        for _ in 0..20 {
            let state = state.clone();
            handles.push(tokio::spawn(async move { call(&state, "/seq").await.0 }));
        }

        let mut counts: HashMap<u16, usize> = HashMap::new();
        for handle in handles {
            let status = handle.await.unwrap();
            *counts.entry(status.as_u16()).or_insert(0) += 1;
        }

        // Each non-repeat step is consumed exactly once, regardless of interleaving
        assert_eq!(counts.get(&201), Some(&1));
        assert_eq!(counts.get(&202), Some(&1));
        assert_eq!(counts.get(&200), Some(&18));
    }

    #[tokio::test]
    async fn test_sequence_counter_survives_hot_reload() {
        let steps = vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))];
        let state = sequence_state("/seq", steps.clone());

        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);

        // Simulate the hot-reload task wholesale-replacing the mock map
        let fresh = HashMap::from([("GET:/seq".to_string(), vec![sequence_mock("/seq", steps)])]);
        *state.mocks.write().await = fresh;

        // Counter survives: next call serves step 2
        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["n"], 2);
    }

    // =========================================================================
    // Response Delay Tests
    // =========================================================================

    fn delayed_state(path: &str, delay: DelayConfig) -> AppState {
        let mut mock = sequence_mock(path, vec![]);
        mock.sequence = None;
        mock.delay_ms = Some(delay);
        let mocks = HashMap::from([(format!("GET:{}", path), vec![mock])]);
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    #[tokio::test]
    async fn test_mock_level_delay_applied() {
        let state = delayed_state("/slow", DelayConfig::Fixed(30));

        let start = std::time::Instant::now();
        let (status, body) = call(&state, "/slow").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["fallback"], true);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "expected at least 30ms delay, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_range_delay_applied() {
        let state = delayed_state("/flaky", DelayConfig::Range { min: 20, max: 40 });

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/flaky").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(20),
            "expected at least 20ms delay, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_step_delay_overrides_mock_delay() {
        let mut step_with_delay = step(200, json!({"ok": true}));
        step_with_delay.delay_ms = Some(10);
        let mut mock = sequence_mock("/seq", vec![step_with_delay]);
        mock.delay_ms = Some(DelayConfig::Fixed(5000));
        let mocks = HashMap::from([("GET:/seq".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(10),
            "expected at least the step's 10ms delay, got {:?}",
            elapsed
        );
        assert!(
            elapsed < std::time::Duration::from_millis(5000),
            "mock-level 5000ms delay should have been overridden, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_mock_delay_applies_to_step_without_own_delay() {
        let mut mock = sequence_mock("/seq", vec![step(201, json!({"n": 1}))]);
        mock.delay_ms = Some(DelayConfig::Fixed(30));
        let mocks = HashMap::from([("GET:/seq".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "expected the mock-level 30ms delay, got {:?}",
            start.elapsed()
        );
    }

    // =========================================================================
    // Custom Response Header Tests
    // =========================================================================

    async fn call_raw(state: &AppState, path: &str) -> (StatusCode, HeaderMap, String) {
        let response = handle_request(
            Method::GET,
            path.parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        (parts.status, parts.headers, text)
    }

    fn headers_state(
        path: &str,
        response: serde_json::Value,
        headers: &[(&str, &str)],
    ) -> AppState {
        let mut mock = sequence_mock(path, vec![]);
        mock.sequence = None;
        mock.response = response;
        mock.response_headers = Some(
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        let mocks = HashMap::from([(format!("GET:{}", path), vec![mock])]);
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    #[tokio::test]
    async fn test_custom_response_headers_present() {
        let state = headers_state(
            "/data",
            json!({"ok": true}),
            &[
                ("X-Custom-Header", "my-value"),
                ("Cache-Control", "no-cache"),
            ],
        );

        let (status, headers, body) = call_raw(&state, "/data").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("x-custom-header").unwrap(), "my-value");
        assert_eq!(headers.get("cache-control").unwrap(), "no-cache");
        // Default Content-Type still applied when not overridden
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(body, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn test_content_type_override_not_doubled() {
        let state = headers_state(
            "/data.xml",
            json!({"ignored": true}),
            &[("Content-Type", "application/xml; charset=utf-8")],
        );

        let (_, headers, _) = call_raw(&state, "/data.xml").await;
        let values: Vec<_> = headers.get_all("content-type").iter().collect();
        assert_eq!(values.len(), 1, "content-type must not be duplicated");
        assert_eq!(values[0], "application/xml; charset=utf-8");
    }

    #[tokio::test]
    async fn test_content_type_override_case_insensitive() {
        // Lowercase key in the config must still suppress the JSON default
        let state = headers_state("/text", json!("hello"), &[("content-type", "text/plain")]);

        let (_, headers, _) = call_raw(&state, "/text").await;
        let values: Vec<_> = headers.get_all("content-type").iter().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], "text/plain");
    }

    #[tokio::test]
    async fn test_non_json_content_type_sends_raw_string_body() {
        let state = headers_state(
            "/data.xml",
            json!("<users><user id=\"1\"/></users>"),
            &[("Content-Type", "application/xml; charset=utf-8")],
        );

        let (status, _, body) = call_raw(&state, "/data.xml").await;
        assert_eq!(status, StatusCode::OK);
        // Raw XML, not a JSON-quoted string
        assert_eq!(body, r#"<users><user id="1"/></users>"#);
    }

    #[tokio::test]
    async fn test_string_response_without_custom_headers_stays_json() {
        let mut mock = sequence_mock("/greeting", vec![]);
        mock.sequence = None;
        mock.response = json!("hello");
        let mocks = HashMap::from([("GET:/greeting".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let (_, headers, body) = call_raw(&state, "/greeting").await;
        // Backward compat: JSON-quoted string with the JSON content type
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(body, r#""hello""#);
    }

    #[tokio::test]
    async fn test_invalid_header_skipped_gracefully() {
        let state = headers_state(
            "/data",
            json!({"ok": true}),
            &[("bad header name", "x"), ("X-Valid", "yes")],
        );

        let (status, headers, _) = call_raw(&state, "/data").await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("bad header name").is_none());
        assert_eq!(headers.get("x-valid").unwrap(), "yes");
    }

    #[tokio::test]
    async fn test_cors_and_location_headers() {
        let state = headers_state(
            "/resources",
            json!({"id": 99}),
            &[
                ("Access-Control-Allow-Origin", "*"),
                ("Location", "/resources/99"),
            ],
        );

        let (_, headers, _) = call_raw(&state, "/resources").await;
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
        assert_eq!(headers.get("location").unwrap(), "/resources/99");
    }

    #[tokio::test]
    async fn test_response_headers_apply_to_sequence_steps() {
        let mut mock = sequence_mock("/seq", vec![step(429, json!({"error": "rate limited"}))]);
        mock.response_headers = Some(HashMap::from([(
            "Retry-After".to_string(),
            "60".to_string(),
        )]));
        let mocks = HashMap::from([("GET:/seq".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let (status, headers, _) = call_raw(&state, "/seq").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(headers.get("retry-after").unwrap(), "60");
    }
}
