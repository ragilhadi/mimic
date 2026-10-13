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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
// HEAD answered from the GET mock (#112)
// =========================================================================

/// Serialized access to the process environment, shared with the matcher
/// tests: a `HEAD` request's outcome depends on `MIMIC_HEAD_FROM_GET`'s
/// default, which the matcher test module's env test flips. See
/// [`crate::types::env_lock`].
async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    crate::types::env_lock().lock().await
}

/// The byte length of the body a GET to `method path` returns on `state`.
async fn get_body_length(state: &AppState, method: Method, path: &str) -> usize {
    let response = handle_request(
        method,
        path.parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .len()
}

#[tokio::test]
async fn test_handle_request_head_answered_from_the_get_mock() {
    let _env = env_guard().await;
    let state = create_test_state();
    let expected_len = get_body_length(&state, Method::GET, "/users").await;

    let response = handle_request(
        Method::HEAD,
        "/users".parse().unwrap(),
        HeaderMap::new(),
        State(state),
        Body::empty(),
    )
    .await;

    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(parts.status, StatusCode::OK);
    assert!(
        bytes.is_empty(),
        "the fallback serves the GET's headers with no body"
    );
    assert_eq!(
        parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .unwrap(),
        expected_len.to_string(),
        "Content-Length reports what GET would have sent"
    );
    assert_eq!(
        parts
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap(),
        "application/json",
        "the GET's content type comes along"
    );
}

#[tokio::test]
async fn test_handle_request_explicit_head_mock_is_unaffected() {
    let _env = env_guard().await;
    let mut mocks = HashMap::new();
    mocks.insert(
        "HEAD:/users".to_string(),
        vec![MockConfig {
            method: "HEAD".to_string(),
            path: "/users".to_string(),
            status: 202,
            response: json!({"queued": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );
    let response = handle_request(
        Method::HEAD,
        "/users".parse().unwrap(),
        HeaderMap::new(),
        State(AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))),
        Body::empty(),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "the explicit HEAD mock serves the request, not any GET it shadows"
    );
}

#[tokio::test]
async fn test_handle_request_head_fallback_serves_a_pattern_route() {
    let _env = env_guard().await;
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
    let expected_len = get_body_length(&state, Method::GET, "/users/42").await;

    let response = handle_request(
        Method::HEAD,
        "/users/42".parse().unwrap(),
        HeaderMap::new(),
        State(state),
        Body::empty(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let (parts, body) = response.into_parts();
    assert!(body.collect().await.unwrap().to_bytes().is_empty());
    assert_eq!(
        parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .unwrap(),
        expected_len.to_string(),
        "HEAD /users/42 is served by GET /users/:id with the GET's size"
    );
}

#[tokio::test]
async fn test_handle_request_head_fallback_bumps_the_get_mocks_state() {
    let _env = env_guard().await;
    let mut mocks = HashMap::new();
    mocks.insert(
        "GET:/users".to_string(),
        vec![MockConfig {
            method: "GET".to_string(),
            path: "/users".to_string(),
            status: 200,
            response: json!({"users": []}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: Some("mocks/users.json".to_string()),
            source_index: None,
            sequence: Some(vec![
                SequenceStep {
                    status: 503,
                    response: json!({"error": "unavailable"}),
                    delay_ms: None,
                    response_file: None,
                    template: None,
                    response_bytes: None,
                    template_escape: None,
                    repeat: false,
                },
                SequenceStep {
                    status: 200,
                    response: json!({"ok": true}),
                    delay_ms: None,
                    response_file: None,
                    template: None,
                    response_bytes: None,
                    template_escape: None,
                    repeat: true,
                },
            ]),
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    // The HEAD is a call: it consumes sequence step 0 and counts a hit.
    let response = handle_request(
        Method::HEAD,
        "/users".parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // ...and the next GET continues the sequence from step 1.
    let response = handle_request(
        Method::GET,
        "/users".parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let identity = {
        let mocks = state.mocks.read().await;
        MockIdentity::of(&mocks["GET:/users"][0], "GET:/users", 0)
    };
    assert_eq!(
        *state.mock_hits.read().await.get(&identity).unwrap(),
        2,
        "both calls were served by the GET mock: HEAD (the fallback) then GET"
    );
}

#[tokio::test]
async fn test_handle_request_head_fallback_is_recorded_as_a_head() {
    let _env = env_guard().await;
    let state = create_test_state();

    let response = handle_request(
        Method::HEAD,
        "/users".parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // The GET body's size, the same way build_response_parts serializes it.
    let expected_len = serde_json::to_string(&json!({"users": [{"id": 1, "name": "Alice"}]}))
        .unwrap()
        .len();

    let log = state.request_log.read().await;
    let record = log.back().unwrap();
    assert_eq!(
        record.method, "HEAD",
        "the log records the request as it came in"
    );
    assert_eq!(record.matched_mock.as_deref(), Some("HEAD:/users"));
    let explanation = record.match_explanation.as_deref().unwrap();
    assert!(
        explanation.contains("HEAD answered from the GET mock at `GET:/users`"),
        "{}",
        explanation
    );
    assert_eq!(
        record.response_body.as_deref(),
        Some(
            format!(
                "<{} bytes of application/json — body withheld for HEAD>",
                expected_len
            )
            .as_str()
        ),
        "the body the client never got is stored as a descriptor"
    );
    assert_eq!(
        record.response_headers.get("content-length"),
        Some(&expected_len.to_string()),
        "the response headers the client got are what the log keeps"
    );
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    // Should match with correct query param
    let method = Method::GET;
    let uri = "/search?q=test".parse().unwrap();
    let headers = HeaderMap::new();
    let response = handle_request(method, uri, headers, State(state.clone()), Body::empty()).await;
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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

/// #85, end to end: `+` is a space on the way into templating, so a
/// response echoing `{{query.q}}` returns what the client meant rather
/// than the wire encoding of it.
#[tokio::test]
async fn test_template_query_param_decodes_plus_as_a_space() {
    let mut mocks = HashMap::new();
    mocks.insert(
        "GET:/search".to_string(),
        vec![MockConfig {
            method: "GET".to_string(),
            path: "/search".to_string(),
            status: 200,
            response: json!({"echo": "{{query.q}}"}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    for query in ["q=hello+world", "q=hello%20world"] {
        let uri = format!("/search?{}", query).parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["echo"], "hello world", "for query '{}'", query);
    }
}

/// The reported repro: a form matcher written in decoded form has to
/// accept the `+`-encoded body a browser or `curl -d` actually sends, and
/// `{{body.*}}` has to render the decoded value.
#[tokio::test]
async fn test_form_body_with_a_plus_encoded_space_matches_and_templates() {
    let mut mocks = HashMap::new();
    mocks.insert(
        "POST:/form/login".to_string(),
        vec![MockConfig {
            method: "POST".to_string(),
            path: "/form/login".to_string(),
            status: 200,
            response: json!({"ok": true, "password_seen": "{{body.password}}"}),
            consume_body: true,
            query_params: None,
            headers: None,
            body: Some(BodyMatcher::Form(crate::types::FormBodyMatcher {
                fields: HashMap::from([
                    ("username".to_string(), "alice".to_string()),
                    ("password".to_string(), "secret pass".to_string()),
                ]),
                strict: false,
            })),
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    for raw in [
        "username=alice&password=secret+pass",
        "username=alice&password=secret%20pass",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        let response = handle_request(
            Method::POST,
            "/form/login".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::from(raw),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "'{}' should match the form matcher",
            raw
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["password_seen"], "secret pass");
    }
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: Some(vec![SequenceStep {
                status: 200,
                response: json!({"echoed": "{{body.message}}"}),
                delay_ms: None,
                response_file: None,
                template: None,
                response_bytes: None,
                template_escape: None,
                repeat: true,
            }]),
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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

#[tokio::test]
async fn test_faker_templates_render_fresh_values_per_call() {
    let mut mocks = HashMap::new();
    mocks.insert(
        "GET:/users/random".to_string(),
        vec![MockConfig {
            method: "GET".to_string(),
            path: "/users/random".to_string(),
            status: 200,
            response: json!({
                "id": "{{faker.uuid}}",
                "name": "{{faker.name}}",
                "age": "{{faker.int min=18 max=99}}",
                "verified": "{{faker.bool}}"
            }),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    let mut ids = Vec::new();
    for _ in 0..5 {
        let response = handle_request(
            Method::GET,
            "/users/random".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let age: i64 = json["age"].as_str().unwrap().parse().unwrap();
        assert!((18..=99).contains(&age), "{} out of range", age);
        assert!(matches!(
            json["verified"].as_str().unwrap(),
            "true" | "false"
        ));
        assert!(json["name"].as_str().unwrap().contains(' '));
        ids.push(json["id"].as_str().unwrap().to_string());
    }

    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "uuid repeated across calls");
}

#[tokio::test]
async fn test_faker_template_in_sequence_step_response() {
    let mut mocks = HashMap::new();
    mocks.insert(
        "POST:/jobs".to_string(),
        vec![MockConfig {
            method: "POST".to_string(),
            path: "/jobs".to_string(),
            status: 200,
            response: json!({"ok": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: Some(vec![SequenceStep {
                status: 202,
                response: json!({"job_id": "{{faker.uuid}}"}),
                delay_ms: None,
                response_file: None,
                template: None,
                response_bytes: None,
                template_escape: None,
                repeat: true,
            }]),
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    let response = handle_request(
        Method::POST,
        "/jobs".parse().unwrap(),
        HeaderMap::new(),
        State(state),
        Body::empty(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["job_id"].as_str().unwrap().len(), 36);
}

#[tokio::test]
async fn test_faker_is_never_used_for_matching() {
    // Templating fires only after a mock is selected, so a `{{faker.…}}`
    // expression sitting in a matcher stays a literal string to match on.
    let mut params = HashMap::new();
    params.insert(
        "token".to_string(),
        QueryParamValue::Exact("{{faker.uuid}}".to_string()),
    );
    let mut mocks = HashMap::new();
    mocks.insert(
        "GET:/guarded".to_string(),
        vec![MockConfig {
            method: "GET".to_string(),
            path: "/guarded".to_string(),
            status: 200,
            response: json!({"ok": true}),
            consume_body: false,
            query_params: Some(QueryParamMatcher {
                params,
                strict: false,
            }),
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    // A real UUID does not satisfy the literal matcher.
    let response = handle_request(
        Method::GET,
        "/guarded?token=3fa85f64-5717-4562-b3fc-2c963f66afa6"
            .parse()
            .unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The literal value does.
    let response = handle_request(
        Method::GET,
        "/guarded?token=%7B%7Bfaker.uuid%7D%7D".parse().unwrap(),
        HeaderMap::new(),
        State(state),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: Some(vec![
                SequenceStep {
                    status: 503,
                    response: json!({"error": "unavailable"}),
                    delay_ms: None,
                    response_file: None,
                    template: None,
                    response_bytes: None,
                    template_escape: None,
                    repeat: false,
                },
                SequenceStep {
                    status: 200,
                    response: json!({"ok": true}),
                    delay_ms: None,
                    response_file: None,
                    template: None,
                    response_bytes: None,
                    template_escape: None,
                    repeat: true,
                },
            ]),
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
            source: None,
            source_index: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
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
                source: None,
                source_index: None,
                sequence: None,
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
                template_escape: None,
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
                source: None,
                source_index: None,
                sequence: None,
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
                template_escape: None,
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
        ..Default::default()
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
        method: Some("POST".to_string()),
        ..Default::default()
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
        status: Some("404".to_string()),
        ..Default::default()
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
        source: None,
        source_index: None,
        sequence: Some(steps),
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

fn step(status: u16, body: serde_json::Value) -> SequenceStep {
    SequenceStep {
        status,
        response: body,
        delay_ms: None,
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
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
        source: None,
        source_index: None,
        sequence: Some(steps),
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
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

fn headers_state(path: &str, response: serde_json::Value, headers: &[(&str, &str)]) -> AppState {
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
#[tokio::test]
async fn test_oversized_body_rejected_with_413() {
    let state = create_test_state();
    let max = max_body_size();

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());

    let response = handle_request(
        Method::POST,
        "/echo".parse().unwrap(),
        headers,
        State(state),
        Body::from(vec![b'x'; max + 1]),
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"], "payload too large");
    assert_eq!(json["max_body_size"], max);
}

#[tokio::test]
async fn test_body_at_the_limit_is_still_accepted() {
    let state = create_test_state();
    let max = max_body_size();

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/plain".parse().unwrap());

    let response = handle_request(
        Method::POST,
        "/echo".parse().unwrap(),
        headers,
        State(state),
        Body::from(vec![b'x'; max]),
    )
    .await;

    // No mock matches /echo, but the body itself was accepted rather than
    // rejected as oversized.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_oversized_streaming_body_is_not_fully_buffered() {
    let state = create_test_state();
    let max = max_body_size();
    let chunk_size = 64 * 1024;

    // A chunked body with no Content-Length that would keep producing far
    // more than the limit. `Limited` must stop polling it once the cap is
    // crossed, so `delivered` never approaches the full (10x limit) size.
    let delivered = Arc::new(AtomicU64::new(0));
    let body = Body::new(EndlessBody {
        delivered: delivered.clone(),
        chunk_size,
        total: max * 10,
    });

    let response = handle_request(
        Method::POST,
        "/echo".parse().unwrap(),
        HeaderMap::new(),
        State(state),
        body,
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let read = delivered.load(Ordering::Relaxed) as usize;
    assert!(
        read <= max + chunk_size,
        "read {} bytes for a {}-byte limit; the body was buffered past the cap",
        read,
        max
    );
}

/// A Content-Length-less body that yields `chunk_size` chunks until
/// `total` bytes have been handed out, counting everything it emits.
struct EndlessBody {
    delivered: Arc<AtomicU64>,
    chunk_size: usize,
    total: usize,
}

impl http_body::Body for EndlessBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        if self.delivered.load(Ordering::Relaxed) as usize >= self.total {
            return std::task::Poll::Ready(None);
        }
        self.delivered
            .fetch_add(self.chunk_size as u64, Ordering::Relaxed);
        std::task::Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from(vec![
                b'x';
                self.chunk_size
            ])))))
    }
}

// ------------------------------------------------------------------
// Per-mock body consumption (#52)
// ------------------------------------------------------------------

fn body_mock(path: &str, consume_body: bool) -> MockConfig {
    MockConfig {
        method: "POST".to_string(),
        path: path.to_string(),
        status: 202,
        response: json!({"queued": true}),
        consume_body,
        query_params: None,
        headers: None,
        body: None,
        delay_ms: None,
        response_headers: None,
        source: None,
        source_index: None,
        sequence: None,
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

async fn post_and_read_recorded_body(
    state: &AppState,
    path: &str,
    payload: &'static str,
) -> Option<String> {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/plain".parse().unwrap());

    let _ = handle_request(
        Method::POST,
        path.parse().unwrap(),
        headers,
        State(state.clone()),
        Body::from(payload),
    )
    .await;

    let response = list_requests(State(state.clone()), Query(RequestFilter::default())).await;
    let requests = response.0["requests"].as_array().unwrap();
    requests
        .last()
        .and_then(|r| r["body"].as_str())
        .map(|s| s.to_string())
}

fn state_with(mocks: Vec<MockConfig>) -> AppState {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for m in mocks {
        map.entry(crate::types::create_mock_key(&m.method, &m.path))
            .or_default()
            .push(m);
    }
    AppState::new(Arc::new(tokio::sync::RwLock::new(map)))
}

#[tokio::test]
async fn test_consume_body_false_skips_reading_the_body() {
    let state = state_with(vec![body_mock("/trigger-job", false)]);
    let recorded = post_and_read_recorded_body(&state, "/trigger-job", "some=body").await;
    assert_eq!(
        recorded, None,
        "consume_body: false must leave the body unread"
    );
}

#[tokio::test]
async fn test_consume_body_true_reads_the_body() {
    let state = state_with(vec![body_mock("/upload", true)]);
    let recorded = post_and_read_recorded_body(&state, "/upload", "file=contents").await;
    assert_eq!(recorded.as_deref(), Some("file=contents"));
}

#[tokio::test]
async fn test_body_matcher_implies_consumption_without_consume_body() {
    let mut mock = body_mock("/match-me", false);
    mock.body = Some(BodyMatcher::Any);
    let state = state_with(vec![mock]);

    let recorded = post_and_read_recorded_body(&state, "/match-me", "payload").await;
    assert_eq!(recorded.as_deref(), Some("payload"));
}

#[tokio::test]
async fn test_body_template_implies_consumption_without_consume_body() {
    let mut mock = body_mock("/echo-body", false);
    mock.response = json!({"echoed": "{{body.field}}"});
    let state = state_with(vec![mock]);

    let recorded = post_and_read_recorded_body(&state, "/echo-body", "field=value").await;
    assert_eq!(recorded.as_deref(), Some("field=value"));
}

#[tokio::test]
async fn test_another_mocks_body_matcher_does_not_force_consumption() {
    // The regression this fixes: `needs_body_matching` used to be global,
    // so an unrelated mock's body matcher made every request buffer.
    let mut unrelated = body_mock("/has-matcher", false);
    unrelated.path = "/has-matcher".to_string();
    unrelated.body = Some(BodyMatcher::Any);

    let state = state_with(vec![unrelated, body_mock("/trigger-job", false)]);

    let recorded = post_and_read_recorded_body(&state, "/trigger-job", "some=body").await;
    assert_eq!(recorded, None);
}

#[tokio::test]
async fn test_unmatched_request_still_records_its_body() {
    // No mock covers this path, so the request 404s either way — capture
    // the body so the request log can show what was actually sent.
    let state = state_with(vec![body_mock("/trigger-job", false)]);
    let recorded = post_and_read_recorded_body(&state, "/nowhere", "debug=me").await;
    assert_eq!(recorded.as_deref(), Some("debug=me"));
}

// ========================================================================
// Admin dashboard v2: /admin/mocks, /admin/sequences, richer records
// ========================================================================

/// A mock with no matchers, loaded from `source`.
fn dash_mock(method: &str, path: &str, source: Option<&str>) -> MockConfig {
    MockConfig {
        method: method.to_string(),
        path: path.to_string(),
        status: 200,
        response: json!({"ok": true}),
        consume_body: false,
        query_params: None,
        headers: None,
        body: None,
        delay_ms: None,
        response_headers: None,
        source: source.map(|s| s.to_string()),
        source_index: None,
        sequence: None,
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

fn dash_state(mocks: Vec<MockConfig>) -> AppState {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for mock in mocks {
        let key = crate::types::create_mock_key(&mock.method, &mock.path);
        map.entry(key).or_default().push(mock);
    }
    AppState::new(Arc::new(tokio::sync::RwLock::new(map)))
}

async fn send(state: &AppState, method: Method, path: &str) -> Response {
    handle_request(
        method,
        path.parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await
}

/// The single recorded request, for tests that make exactly one.
async fn only_record(state: &AppState) -> RequestRecord {
    let log = state.request_log.read().await;
    assert_eq!(log.len(), 1, "expected exactly one recorded request");
    log[0].clone()
}

// ── GET /admin/mocks ────────────────────────────────────────────────

#[tokio::test]
async fn test_list_mocks_reports_every_loaded_mock() {
    let state = dash_state(vec![
        dash_mock("GET", "/users", Some("mocks/get_users.json")),
        dash_mock("POST", "/login", Some("mocks/post_login.json")),
    ]);

    let body = list_mocks(State(state)).await.0;
    assert_eq!(body["count"], 2);

    let mocks = body["mocks"].as_array().unwrap();
    // Sorted by key, so the listing doesn't reshuffle between refreshes
    assert_eq!(mocks[0]["key"], "GET:/users");
    assert_eq!(mocks[0]["method"], "GET");
    assert_eq!(mocks[0]["path"], "/users");
    assert_eq!(mocks[0]["status"], 200);
    assert_eq!(mocks[0]["source"], "mocks/get_users.json");
    assert_eq!(mocks[1]["key"], "POST:/login");
}

#[tokio::test]
async fn test_list_mocks_names_one_element_of_an_array_file() {
    // Two mocks loaded from the same array file keep separate rows, each
    // pointing at its own place in that file.
    let mut first = dash_mock("GET", "/orders", Some("mocks/orders.json"));
    first.source_index = Some(0);
    let mut second = dash_mock("DELETE", "/orders/:id", Some("mocks/orders.json"));
    second.source_index = Some(3);

    let body = list_mocks(State(dash_state(vec![first, second]))).await.0;
    let mocks = body["mocks"].as_array().unwrap();

    // Sorted by key: DELETE before GET.
    assert_eq!(mocks[0]["source"], "mocks/orders.json#3");
    assert_eq!(mocks[1]["source"], "mocks/orders.json#0");
}

#[tokio::test]
async fn test_list_mocks_reports_matchers_and_shape() {
    let mut mock = dash_mock("GET", "/users/:id", Some("mocks/get_user.json"));
    mock.headers = Some(HeaderMatcher {
        required: HashMap::from([(
            "x-api-key".to_string(),
            HeaderValue::Pattern(HeaderPattern::Any),
        )]),
        forbidden: Vec::new(),
        strict: false,
    });
    mock.delay_ms = Some(DelayConfig::Fixed(250));
    mock.response_headers = Some(HashMap::from([("x-trace".to_string(), "on".to_string())]));

    let body = list_mocks(State(dash_state(vec![mock]))).await.0;
    let entry = &body["mocks"][0];

    assert_eq!(entry["has_path_params"], true);
    assert_eq!(entry["matchers"]["headers"], true);
    assert_eq!(entry["matchers"]["query_params"], false);
    assert_eq!(entry["matchers"]["body"], false);
    assert_eq!(entry["delay_ms"], 250);
    assert_eq!(entry["response_headers"], 1);
    assert_eq!(entry["sequence_steps"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_list_mocks_includes_the_full_config() {
    // The Mocks tab expands a row into the mock file's own JSON, so the
    // whole config has to come down with the summary.
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);
    let body = list_mocks(State(state)).await.0;

    let config = &body["mocks"][0]["config"];
    assert_eq!(config["method"], "GET");
    assert_eq!(config["path"], "/users");
    assert_eq!(config["response"]["ok"], true);
}

#[tokio::test]
async fn test_list_mocks_renders_a_mock_without_a_source() {
    // A mock built in-process has no file to link to; the field is absent
    // rather than an empty string the UI would have to special-case.
    let state = dash_state(vec![dash_mock("GET", "/users", None)]);
    let body = list_mocks(State(state)).await.0;
    assert_eq!(body["mocks"][0]["source"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_list_mocks_counts_hits() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);

    for _ in 0..3 {
        let _ = send(&state, Method::GET, "/users").await;
    }
    let _ = send(&state, Method::GET, "/nowhere").await;

    let body = list_mocks(State(state)).await.0;
    assert_eq!(body["mocks"][0]["hits"], 3);
}

#[tokio::test]
async fn test_list_mocks_counts_hits_per_mock_not_per_path() {
    // Two mocks share GET:/users; only the one that actually served the
    // request should show a hit.
    let mut picky = dash_mock("GET", "/users", Some("mocks/picky.json"));
    picky.headers = Some(HeaderMatcher {
        required: HashMap::from([(
            "x-api-key".to_string(),
            HeaderValue::Pattern(HeaderPattern::Any),
        )]),
        forbidden: Vec::new(),
        strict: false,
    });
    let state = dash_state(vec![
        picky,
        dash_mock("GET", "/users", Some("mocks/plain.json")),
    ]);

    let _ = send(&state, Method::GET, "/users").await;

    let body = list_mocks(State(state)).await.0;
    let mocks = body["mocks"].as_array().unwrap();
    let plain = mocks
        .iter()
        .find(|m| m["source"] == "mocks/plain.json")
        .unwrap();
    let picky = mocks
        .iter()
        .find(|m| m["source"] == "mocks/picky.json")
        .unwrap();
    assert_eq!(plain["hits"], 1);
    assert_eq!(picky["hits"], 0);
}

#[tokio::test]
async fn test_list_mocks_reflects_a_hot_reload() {
    // Hot reload swaps the map behind the same lock; the endpoint reads
    // through rather than caching, so a reload is visible immediately.
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);
    assert_eq!(list_mocks(State(state.clone())).await.0["count"], 1);

    {
        let mut mocks = state.mocks.write().await;
        mocks.insert(
            "POST:/login".to_string(),
            vec![dash_mock("POST", "/login", Some("mocks/b.json"))],
        );
    }

    let body = list_mocks(State(state)).await.0;
    assert_eq!(body["count"], 2);
}

#[tokio::test]
async fn test_list_mocks_on_an_empty_store() {
    let state = dash_state(vec![]);
    let body = list_mocks(State(state)).await.0;
    assert_eq!(body["count"], 0);
    assert_eq!(body["mocks"].as_array().unwrap().len(), 0);
}

// ── GET /admin/sequences ────────────────────────────────────────────

fn dash_sequence_mock() -> MockConfig {
    let mut mock = dash_mock("POST", "/submit", Some("mocks/post_submit.json"));
    mock.sequence = Some(vec![
        SequenceStep {
            status: 202,
            response: json!({"state": "pending"}),
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
            repeat: false,
        },
        SequenceStep {
            status: 200,
            response: json!({"state": "done"}),
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
            repeat: false,
        },
        SequenceStep {
            status: 200,
            response: json!({"state": "done"}),
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
            repeat: true,
        },
    ]);
    mock
}

#[tokio::test]
async fn test_list_sequences_reports_the_current_step() {
    let state = dash_state(vec![dash_sequence_mock()]);

    let empty = list_sequences(State(state.clone())).await.0;
    assert_eq!(empty["count"], 0, "no counter exists until a request lands");

    let _ = send(&state, Method::POST, "/submit").await;
    let _ = send(&state, Method::POST, "/submit").await;

    let body = list_sequences(State(state)).await.0;
    assert_eq!(body["count"], 1);
    let seq = &body["sequences"][0];
    // The counter is named after the file it belongs to, not its position
    // in a bucket hot reload rebuilds every two seconds.
    assert_eq!(seq["key"], "POST:/submit @ mocks/post_submit.json");
    assert_eq!(seq["method"], "POST");
    assert_eq!(seq["path"], "/submit");
    assert_eq!(seq["step"], 2);
    assert_eq!(seq["total"], 3);
    assert_eq!(seq["source"], "mocks/post_submit.json");
}

#[tokio::test]
async fn test_list_sequences_after_a_reset() {
    let state = dash_state(vec![dash_sequence_mock()]);
    let _ = send(&state, Method::POST, "/submit").await;

    let filter = Query(SequenceResetFilter {
        path: Some("/submit".to_string()),
    });
    let reset = reset_sequences(State(state.clone()), filter).await.0;
    assert_eq!(reset["reset"], 1);

    let body = list_sequences(State(state)).await.0;
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn test_list_sequences_survives_a_counter_with_no_mock() {
    // Hot reload can delete the mock a live counter belongs to. The panel
    // must still render the counter rather than blow up looking for it.
    let state = dash_state(vec![]);
    {
        let mut counters = state.sequence_counters.write().await;
        counters.insert(
            MockIdentity {
                key: "POST:/gone".to_string(),
                origin: crate::types::MockOrigin::Source("mocks/gone.json".to_string(), None),
            },
            4,
        );
    }

    let body = list_sequences(State(state)).await.0;
    assert_eq!(body["count"], 1);
    assert_eq!(body["sequences"][0]["step"], 4);
    assert_eq!(body["sequences"][0]["path"], "/gone");
    assert_eq!(body["sequences"][0]["total"], serde_json::Value::Null);
}

// ── request record: response, score, path params, explanation ───────

#[tokio::test]
async fn test_record_captures_the_response_that_was_served() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);
    let response = send(&state, Method::GET, "/users").await;

    let sent = response.into_body().collect().await.unwrap().to_bytes();
    let record = only_record(&state).await;

    assert_eq!(record.response_status, 200);
    assert_eq!(
        record.response_body.as_deref(),
        Some(String::from_utf8(sent.to_vec()).unwrap().as_str()),
        "the recorded body must be the bytes the client got"
    );
    assert_eq!(
        record
            .response_headers
            .get("content-type")
            .map(String::as_str),
        Some("application/json")
    );
}

/// The #108 repro end to end: `?tag=a&tag=b` must not lose `a` on the way
/// into the request log.
#[tokio::test]
async fn test_record_keeps_every_value_of_a_repeated_query_param() {
    let state = dash_state(vec![dash_mock("GET", "/search", Some("mocks/search.json"))]);
    let _ = send(&state, Method::GET, "/search?tag=a&tag=b").await;

    let record = only_record(&state).await;
    assert_eq!(
        record.query_params.get("tag"),
        Some(&vec!["a".to_string(), "b".to_string()])
    );
}

#[tokio::test]
async fn test_record_captures_score_and_explanation_on_a_match() {
    let state = dash_state(vec![dash_mock(
        "GET",
        "/users",
        Some("mocks/get_users.json"),
    )]);
    let _ = send(&state, Method::GET, "/users").await;

    let record = only_record(&state).await;
    assert_eq!(record.match_score, Some(1000));
    let explanation = record.match_explanation.unwrap();
    assert!(
        explanation.contains("matched mocks/get_users.json"),
        "{}",
        explanation
    );
    assert!(explanation.contains("score 1000"), "{}", explanation);
}

#[tokio::test]
async fn test_record_captures_path_params() {
    let state = dash_state(vec![dash_mock("GET", "/users/:id", Some("mocks/u.json"))]);
    let _ = send(&state, Method::GET, "/users/42").await;

    let record = only_record(&state).await;
    assert_eq!(record.path_params.get("id").map(String::as_str), Some("42"));
    assert_eq!(record.match_score, Some(900));
}

#[tokio::test]
async fn test_record_explains_an_unmatched_request() {
    let mut mock = dash_mock("GET", "/users", Some("mocks/get_users.json"));
    mock.headers = Some(HeaderMatcher {
        required: HashMap::from([(
            "x-api-key".to_string(),
            HeaderValue::Pattern(HeaderPattern::Any),
        )]),
        forbidden: Vec::new(),
        strict: false,
    });
    let state = dash_state(vec![mock]);

    let _ = send(&state, Method::GET, "/users").await;

    let record = only_record(&state).await;
    assert_eq!(record.response_status, 404);
    assert!(record.matched_mock.is_none());
    assert!(record.match_score.is_none());
    let explanation = record.match_explanation.unwrap();
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

#[tokio::test]
async fn test_record_of_a_404_keeps_the_error_body() {
    let state = dash_state(vec![]);
    let _ = send(&state, Method::GET, "/nowhere").await;

    let record = only_record(&state).await;
    let body: serde_json::Value =
        serde_json::from_str(record.response_body.as_deref().unwrap()).unwrap();
    assert_eq!(body["error"], "mock not found");
    assert_eq!(body["path"], "/nowhere");
}

// ── redaction on the new output surfaces ────────────────────────────

#[tokio::test]
async fn test_response_headers_redact_set_cookie() {
    // A mock returning a credential must not have it rendered in the
    // dashboard's new Response section.
    let mut mock = dash_mock("POST", "/login", Some("mocks/login.json"));
    mock.response_headers = Some(HashMap::from([
        ("set-cookie".to_string(), "session=super-secret".to_string()),
        ("x-request-id".to_string(), "abc-123".to_string()),
    ]));
    let state = dash_state(vec![mock]);

    let response = send(&state, Method::POST, "/login").await;
    // The client still gets the real cookie — only the log is redacted
    assert_eq!(
        response.headers().get("set-cookie").unwrap(),
        "session=super-secret"
    );

    let record = only_record(&state).await;
    assert_eq!(
        record
            .response_headers
            .get("set-cookie")
            .map(String::as_str),
        Some("[REDACTED]")
    );
    assert_eq!(
        record
            .response_headers
            .get("x-request-id")
            .map(String::as_str),
        Some("abc-123"),
        "ordinary headers are still useful and must survive"
    );

    let serialized = serde_json::to_string(&record).unwrap();
    assert!(
        !serialized.contains("super-secret"),
        "a credential reached the admin API: {}",
        serialized
    );
}

#[tokio::test]
async fn test_response_headers_redact_authorization_and_cookie() {
    for name in ["authorization", "cookie", "Set-Cookie"] {
        let mut mock = dash_mock("GET", "/echo", Some("mocks/echo.json"));
        mock.response_headers = Some(HashMap::from([(name.to_string(), "leak-me".to_string())]));
        let state = dash_state(vec![mock]);

        let _ = send(&state, Method::GET, "/echo").await;

        let record = only_record(&state).await;
        let serialized = serde_json::to_string(&record).unwrap();
        assert!(
            !serialized.contains("leak-me"),
            "{} leaked into the log: {}",
            name,
            serialized
        );
    }
}

#[tokio::test]
async fn test_explanation_never_carries_a_request_credential() {
    let mut mock = dash_mock("GET", "/vault", Some("mocks/vault.json"));
    mock.headers = Some(HeaderMatcher {
        required: HashMap::from([(
            "authorization".to_string(),
            HeaderValue::Exact("Bearer expected-secret".to_string()),
        )]),
        forbidden: Vec::new(),
        strict: false,
    });
    let state = dash_state(vec![mock]);

    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer sent-secret".parse().unwrap());
    let _ = handle_request(
        Method::GET,
        "/vault".parse().unwrap(),
        headers,
        State(state.clone()),
        Body::empty(),
    )
    .await;

    let record = only_record(&state).await;
    let serialized = serde_json::to_string(&record).unwrap();
    assert!(!serialized.contains("sent-secret"), "{}", serialized);
    assert!(!serialized.contains("expected-secret"), "{}", serialized);
    assert!(record.match_explanation.unwrap().contains("[REDACTED]"));
}

// ── filters ─────────────────────────────────────────────────────────

/// State with one recorded request per (method, path) given.
async fn state_with_traffic(traffic: &[(Method, &str)]) -> AppState {
    let state = dash_state(vec![
        dash_mock("GET", "/users", Some("mocks/users.json")),
        dash_mock("GET", "/users/active", Some("mocks/active.json")),
        dash_mock("POST", "/login", Some("mocks/login.json")),
    ]);
    for (method, path) in traffic {
        let _ = send(&state, method.clone(), path).await;
    }
    state
}

#[tokio::test]
async fn test_path_filter_matches_a_substring() {
    // The placeholder has always read like a search box. Now it is one.
    let state = state_with_traffic(&[
        (Method::GET, "/users"),
        (Method::GET, "/users/active"),
        (Method::POST, "/login"),
    ])
    .await;

    let filter = Query(RequestFilter {
        path: Some("user".to_string()),
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 2);
}

#[tokio::test]
async fn test_path_filter_still_matches_an_exact_path() {
    let state = state_with_traffic(&[(Method::GET, "/users"), (Method::POST, "/login")]).await;

    let filter = Query(RequestFilter {
        path: Some("/login".to_string()),
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 1);
    assert_eq!(body["requests"][0]["path"], "/login");
}

// ── Pagination (#111) ───────────────────────────────────────────────

#[tokio::test]
async fn test_pagination_defaults_to_the_last_50_most_recent_matches() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
    for _ in 0..60 {
        let _ = send(&state, Method::GET, "/users").await;
    }

    let body = list_requests(State(state), Query(RequestFilter::default()))
        .await
        .0;

    assert_eq!(body["count"], 60, "count is every match, not just the page");
    assert_eq!(body["returned"], 50);
    assert_eq!(body["limit"], 50);
    assert_eq!(body["offset"], 0);
    let requests = body["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 50);
    // ids run 1..=60; the default page is the most recent 50: 11..=60.
    assert_eq!(requests[0]["id"], 11);
    assert_eq!(requests[49]["id"], 60);
}

#[tokio::test]
async fn test_pagination_offset_pages_backward_through_history() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
    for _ in 0..5 {
        let _ = send(&state, Method::GET, "/users").await;
    }

    let ids_for = |body: &serde_json::Value| -> Vec<u64> {
        body["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_u64().unwrap())
            .collect()
    };

    let newest = list_requests(
        State(state.clone()),
        Query(RequestFilter {
            limit: Some("2".to_string()),
            offset: Some("0".to_string()),
            ..Default::default()
        }),
    )
    .await
    .0;
    assert_eq!(ids_for(&newest), vec![4, 5]);

    let older = list_requests(
        State(state),
        Query(RequestFilter {
            limit: Some("2".to_string()),
            offset: Some("2".to_string()),
            ..Default::default()
        }),
    )
    .await
    .0;
    assert_eq!(ids_for(&older), vec![2, 3]);
}

#[tokio::test]
async fn test_pagination_limit_zero_returns_everything() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
    for _ in 0..70 {
        let _ = send(&state, Method::GET, "/users").await;
    }

    let body = list_requests(
        State(state),
        Query(RequestFilter {
            limit: Some("0".to_string()),
            ..Default::default()
        }),
    )
    .await
    .0;

    assert_eq!(body["count"], 70);
    assert_eq!(body["returned"], 70);
    assert_eq!(body["requests"].as_array().unwrap().len(), 70);
}

#[tokio::test]
async fn test_pagination_applies_after_filters() {
    let state = dash_state(vec![
        dash_mock("GET", "/users", Some("mocks/users.json")),
        dash_mock("POST", "/login", Some("mocks/login.json")),
    ]);
    for _ in 0..5 {
        let _ = send(&state, Method::GET, "/users").await;
    }
    let _ = send(&state, Method::POST, "/login").await;

    let body = list_requests(
        State(state),
        Query(RequestFilter {
            path: Some("/users".to_string()),
            limit: Some("2".to_string()),
            ..Default::default()
        }),
    )
    .await
    .0;

    assert_eq!(
        body["count"], 5,
        "pagination must be counted against the filtered set, not the whole log"
    );
    assert_eq!(body["returned"], 2);
}

#[tokio::test]
async fn test_status_filter_accepts_a_class() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
    let _ = send(&state, Method::GET, "/users").await; // 200
    let _ = send(&state, Method::GET, "/nope").await; // 404
    let _ = send(&state, Method::GET, "/also-nope").await; // 404

    for (class, expected) in [("4xx", 2), ("2xx", 1), ("5xx", 0)] {
        let filter = Query(RequestFilter {
            status: Some(class.to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state.clone()), filter).await.0;
        assert_eq!(body["count"], expected, "status={}", class);
    }
}

#[tokio::test]
async fn test_status_filter_accepts_an_exact_code() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
    let _ = send(&state, Method::GET, "/users").await;
    let _ = send(&state, Method::GET, "/nope").await;

    let filter = Query(RequestFilter {
        status: Some("404".to_string()),
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 1);
    assert_eq!(body["requests"][0]["response_status"], 404);
}

#[tokio::test]
async fn test_unmatched_only_filter() {
    let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
    let _ = send(&state, Method::GET, "/users").await;
    let _ = send(&state, Method::GET, "/nope").await;

    let filter = Query(RequestFilter {
        unmatched_only: Some("true".to_string()),
        ..Default::default()
    });
    let body = list_requests(State(state.clone()), filter).await.0;
    assert_eq!(body["count"], 1);
    assert_eq!(body["requests"][0]["path"], "/nope");

    // An empty value reads as "off" rather than failing the request
    let filter = Query(RequestFilter {
        unmatched_only: Some(String::new()),
        ..Default::default()
    });
    assert_eq!(list_requests(State(state), filter).await.0["count"], 2);
}

#[tokio::test]
async fn test_search_filter_looks_in_the_body() {
    let state = dash_state(vec![]);
    for body in ["needle in here", "nothing to see"] {
        let _ = handle_request(
            Method::POST,
            "/inbox".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::from(body),
        )
        .await;
    }

    let filter = Query(RequestFilter {
        search: Some("NEEDLE".to_string()), // case-insensitive
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 1);
}

#[tokio::test]
async fn test_search_filter_looks_in_the_headers() {
    let state = dash_state(vec![]);
    let mut headers = HeaderMap::new();
    headers.insert("x-tenant", "acme-corp".parse().unwrap());
    let _ = handle_request(
        Method::GET,
        "/a".parse().unwrap(),
        headers,
        State(state.clone()),
        Body::empty(),
    )
    .await;
    let _ = send(&state, Method::GET, "/b").await;

    let filter = Query(RequestFilter {
        search: Some("acme".to_string()),
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 1);
    assert_eq!(body["requests"][0]["path"], "/a");
}

#[tokio::test]
async fn test_filters_combine() {
    let state = state_with_traffic(&[
        (Method::GET, "/users"),
        (Method::GET, "/users/active"),
        (Method::POST, "/login"),
    ])
    .await;

    let filter = Query(RequestFilter {
        path: Some("user".to_string()),
        method: Some("GET".to_string()),
        status: Some("2xx".to_string()),
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 2);
}

#[tokio::test]
async fn test_empty_filters_are_ignored() {
    // The dashboard omits empty boxes, but a hand-written curl may not.
    let state = state_with_traffic(&[(Method::GET, "/users"), (Method::POST, "/login")]).await;

    let filter = Query(RequestFilter {
        path: Some(String::new()),
        method: Some(String::new()),
        status: Some(String::new()),
        search: Some("   ".to_string()),
        unmatched_only: None,
        ..Default::default()
    });
    let body = list_requests(State(state), filter).await.0;
    assert_eq!(body["count"], 2);
}

#[test]
fn test_status_matches() {
    assert!(status_matches("404", 404));
    assert!(!status_matches("404", 400));
    assert!(status_matches("4xx", 404));
    assert!(status_matches("4XX", 451));
    assert!(!status_matches("4xx", 500));
    assert!(status_matches("5xx", 503));
    assert!(status_matches("", 200), "an empty filter filters nothing");
    // A filter that can't be understood matches nothing, so a typo shows an
    // empty table rather than a full one that looks unfiltered.
    assert!(!status_matches("nonsense", 200));
    assert!(!status_matches("xx", 200));
}

#[test]
fn test_is_truthy() {
    for yes in ["1", "true", "TRUE", " yes ", "on"] {
        assert!(is_truthy(yes), "{}", yes);
    }
    for no in ["", "0", "false", "no", "maybe"] {
        assert!(!is_truthy(no), "{}", no);
    }
}

// ── bounded log and truncated bodies ────────────────────────────────

#[test]
fn test_truncate_body() {
    assert_eq!(truncate_body("short".to_string(), 10), "short");
    assert_eq!(truncate_body("exactly10!".to_string(), 10), "exactly10!");

    let truncated = truncate_body("0123456789abc".to_string(), 10);
    assert_eq!(truncated, format!("0123456789{}", TRUNCATION_MARKER));

    // 0 opts out entirely
    let long = "x".repeat(5000);
    assert_eq!(truncate_body(long.clone(), 0), long);
}

#[test]
fn test_truncate_body_respects_character_boundaries() {
    // Cutting a multi-byte character in half would make the record
    // unserializable; the cut walks back to a boundary instead.
    let body = "héllo wörld".to_string();
    for limit in 1..body.len() {
        let truncated = truncate_body(body.clone(), limit);
        assert!(
            truncated.ends_with(TRUNCATION_MARKER),
            "limit {} should truncate",
            limit
        );
        // Round-trips through JSON, which only accepts valid UTF-8
        assert!(serde_json::to_string(&truncated).is_ok());
    }
}

#[test]
fn test_push_bounded_drops_the_oldest() {
    let mut log: std::collections::VecDeque<RequestRecord> = Default::default();
    for id in 1..=10 {
        let record = RequestRecord {
            id,
            ..Default::default()
        };
        push_bounded(&mut log, record, 3);
    }

    assert_eq!(log.len(), 3);
    assert_eq!(log.iter().map(|r| r.id).collect::<Vec<_>>(), vec![8, 9, 10]);
}

#[test]
fn test_push_bounded_with_no_cap() {
    let mut log: std::collections::VecDeque<RequestRecord> = Default::default();
    for id in 1..=50 {
        push_bounded(
            &mut log,
            RequestRecord {
                id,
                ..Default::default()
            },
            0,
        );
    }
    assert_eq!(log.len(), 50);
}

#[tokio::test]
async fn test_a_large_response_body_is_truncated_in_the_log() {
    let mut mock = dash_mock("GET", "/big", Some("mocks/big.json"));
    mock.response = json!({"blob": "x".repeat(200_000)});
    let state = dash_state(vec![mock]);

    let response = send(&state, Method::GET, "/big").await;
    let sent = response.into_body().collect().await.unwrap().to_bytes();
    assert!(sent.len() > 100_000, "the client still gets the whole body");

    let record = only_record(&state).await;
    let stored = record.response_body.unwrap();
    assert!(
        stored.len() < sent.len(),
        "the log kept the whole {}-byte body",
        sent.len()
    );
    assert!(stored.ends_with(TRUNCATION_MARKER));
}

// ── /health summary ─────────────────────────────────────────────────

#[tokio::test]
async fn test_health_reports_the_dashboard_summary() {
    let state = dash_state(vec![
        dash_mock("GET", "/users", Some("mocks/a.json")),
        dash_mock("POST", "/login", Some("mocks/b.json")),
    ]);
    let _ = send(&state, Method::GET, "/users").await;

    let body = health_check(State(state)).await.0;
    assert_eq!(body["status"], "healthy");
    assert_eq!(body["mocks_loaded"], 2);
    assert_eq!(body["mock_count"], 2);
    assert_eq!(body["requests_recorded"], 1);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["port"].is_number());
    assert!(body["max_body_size"].is_number());
    assert!(body["uptime_seconds"].is_number());
}

#[tokio::test]
async fn test_health_counts_mocks_behind_a_shared_route() {
    // Two mocks, one route: `mocks_loaded` keeps its old meaning and
    // `mock_count` reports the definitions.
    let state = dash_state(vec![
        dash_mock("GET", "/users", Some("mocks/a.json")),
        dash_mock("GET", "/users", Some("mocks/b.json")),
    ]);
    let body = health_check(State(state)).await.0;
    assert_eq!(body["mocks_loaded"], 1);
    assert_eq!(body["mock_count"], 2);
}

// ── the dashboard page itself ───────────────────────────────────────

#[tokio::test]
async fn test_dashboard_html_wires_up_the_new_surfaces() {
    let html = admin_dashboard().await.0;
    for needle in [
        "/admin/mocks",
        "/admin/sequences",
        "unmatched_only=true",
        "prefers-color-scheme",
        "data-tab=\"mocks\"",
        "data-tab=\"sequences\"",
    ] {
        assert!(html.contains(needle), "dashboard is missing {}", needle);
    }
}

#[tokio::test]
async fn test_dashboard_html_wires_up_the_token_prompt() {
    let html = admin_dashboard().await.0;
    for needle in [
        "auth-overlay",
        "authFetch(",
        "sessionStorage",
        "Authorization': 'Bearer '",
        "?token=",
        "history.replaceState",
        "mimic-admin-token",
    ] {
        assert!(
            html.contains(needle),
            "dashboard's token wiring is missing {}",
            needle
        );
    }
}

// ========================================================================
// Proxy / record-and-replay (#60)
// ========================================================================

mod proxy_tests {
    use super::*;
    use crate::proxy::ProxyConfig;
    use axum::routing::get;
    use axum::Router;
    use tokio::net::TcpListener;

    /// A tiny upstream that counts hits and answers `GET /widgets/1`
    /// with a fixed JSON body. Returns its base URL and the shared hit
    /// counter.
    async fn spawn_counting_upstream() -> (String, Arc<AtomicU64>) {
        let hits = Arc::new(AtomicU64::new(0));
        let counter = hits.clone();

        let app = Router::new().route(
            "/widgets/1",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"id": 1, "name": "Widget"}))
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{}", addr), hits)
    }

    /// An upstream that accepts the connection and then never responds,
    /// so a client-side timeout is what ends the request.
    async fn spawn_hanging_upstream() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    // Hold the connection open without ever writing a
                    // response; the client's own timeout must fire.
                    std::mem::forget(socket);
                }
            }
        });
        format!("http://{}", addr)
    }

    fn proxy_state(mocks_dir: &std::path::Path, upstream: &str, record: bool) -> AppState {
        let mut state = create_empty_state();
        state.mocks_dir = mocks_dir.to_str().unwrap().to_string();
        state.proxy_config = Some(Arc::new(ProxyConfig::for_test(
            upstream.to_string(),
            record,
            2000,
        )));
        state
    }

    #[tokio::test]
    async fn unmatched_request_is_forwarded_to_the_upstream_and_the_response_passed_through() {
        let (upstream, hits) = spawn_counting_upstream().await;
        let dir = tempfile::tempdir().unwrap();
        let state = proxy_state(dir.path(), &upstream, false);

        let response = handle_request(
            Method::GET,
            "/widgets/1".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["name"], "Widget");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upstream_timeout_falls_back_to_a_404_with_an_upstream_error_field() {
        let upstream = spawn_hanging_upstream().await;
        let dir = tempfile::tempdir().unwrap();
        let mut state = create_empty_state();
        state.mocks_dir = dir.path().to_str().unwrap().to_string();
        state.proxy_config = Some(Arc::new(ProxyConfig::for_test(upstream, false, 100)));

        let response = handle_request(
            Method::GET,
            "/widgets/1".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "mock not found");
        assert!(
            json["upstream_error"].is_string(),
            "a timed-out proxy attempt must surface why: {}",
            json
        );
    }

    #[tokio::test]
    async fn reserved_health_and_admin_routes_are_never_proxied() {
        // Proxying is configured, and neither path exists as a mock —
        // if the reservation check were missing, these would reach the
        // (nonexistent) upstream route instead of the ordinary 404 shape.
        // Only the exact method+path pairs `ReservedRoutes` protects are
        // covered here; an admin-adjacent path Mimic doesn't itself
        // answer (e.g. `/admin/not-a-real-route`) is deliberately *not*
        // reserved — see `ReservedRoutes::reservation_for` — and is free
        // to be proxied like any other unmocked request.
        let (upstream, hits) = spawn_counting_upstream().await;
        let dir = tempfile::tempdir().unwrap();
        let state = proxy_state(dir.path(), &upstream, false);

        for path in ["/health", "/admin/dashboard"] {
            let response = handle_request(
                Method::GET,
                path.parse().unwrap(),
                HeaderMap::new(),
                State(state.clone()),
                Body::empty(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"], "mock not found");
            assert!(
                json.get("upstream_error").is_none(),
                "{} must never reach the proxy",
                path
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The full record-and-replay loop from the issue's acceptance
    /// criteria: a proxied response is written to `mocks/_recorded/`,
    /// and once that file is loaded back in (simulating the hot-reload
    /// main.rs runs every few seconds), the *second* identical request
    /// is answered locally — the upstream must see exactly one hit.
    #[tokio::test]
    async fn recorded_response_is_replayed_on_the_next_identical_request_without_a_second_upstream_hit(
    ) {
        let (upstream, hits) = spawn_counting_upstream().await;
        let dir = tempfile::tempdir().unwrap();
        let state = proxy_state(dir.path(), &upstream, true);

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer super-secret-token".parse().unwrap(),
        );

        let first = handle_request(
            Method::GET,
            "/widgets/1".parse().unwrap(),
            headers.clone(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // The write happens on a detached task; poll briefly for the
        // recorded file to land instead of guessing a fixed sleep.
        let recorded_dir = dir.path().join("_recorded");
        let mut recorded_file = None;
        for _ in 0..50 {
            if recorded_dir.is_dir() {
                let mut entries: Vec<_> = std::fs::read_dir(&recorded_dir)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .collect();
                if !entries.is_empty() {
                    recorded_file = Some(entries.remove(0).path());
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let recorded_file = recorded_file.expect("a mock should have been recorded");

        let contents = std::fs::read_to_string(&recorded_file).unwrap();
        assert!(
            !contents.contains("super-secret-token"),
            "the Authorization header value must never reach disk: {}",
            contents
        );

        // Simulate the hot-reload main.rs runs on an interval: load what
        // landed on disk into a fresh mock store.
        let loaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(loaded.errors.len(), 0);
        {
            let mut mocks = state.mocks.write().await;
            *mocks = loaded.mocks;
        }

        let second = handle_request(
            Method::GET,
            "/widgets/1".parse().unwrap(),
            headers,
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(second.status(), StatusCode::OK);
        let bytes = second.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["name"], "Widget");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the second identical request must be served from the recorded mock, not the upstream again"
        );
    }

    /// An upstream that answers `GET /gz` with a gzip-compressed JSON
    /// body and `content-encoding: gzip` — the #106 repro.
    async fn spawn_gzip_upstream() -> String {
        let app = Router::new().route(
            "/gz",
            get(|| async {
                use std::io::Write;
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder
                    .write_all(br#"{"gzipped":true,"value":42}"#)
                    .unwrap();
                let body = encoder.finish().unwrap();
                (
                    [
                        (
                            CONTENT_TYPE,
                            axum::http::HeaderValue::from_static("application/json"),
                        ),
                        (
                            axum::http::header::CONTENT_ENCODING,
                            axum::http::HeaderValue::from_static("gzip"),
                        ),
                    ],
                    body,
                )
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        format!("http://{}", addr)
    }

    /// The #106 repro end to end: the live response is decoded
    /// correctly (unsurprising — that part already worked), and the
    /// *recording* is the decoded JSON too, not mojibake, with no
    /// `content-encoding` claiming a compression the stored bytes no
    /// longer have.
    #[tokio::test]
    async fn a_gzipped_upstream_response_is_recorded_decoded_not_as_mojibake() {
        let upstream = spawn_gzip_upstream().await;
        let dir = tempfile::tempdir().unwrap();
        let state = proxy_state(dir.path(), &upstream, true);

        let response = handle_request(
            Method::GET,
            "/gz".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let live: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(live["gzipped"], true);
        assert_eq!(live["value"], 42);

        let recorded_dir = dir.path().join("_recorded");
        let mut recorded_file = None;
        for _ in 0..50 {
            if recorded_dir.is_dir() {
                let mut entries: Vec<_> = std::fs::read_dir(&recorded_dir)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .collect();
                if !entries.is_empty() {
                    recorded_file = Some(entries.remove(0).path());
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let recorded_file = recorded_file.expect("a mock should have been recorded");

        let recorded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&recorded_file).unwrap()).unwrap();
        assert_eq!(
            recorded["response"],
            json!({"gzipped": true, "value": 42}),
            "the recorded response must be the decoded body, not mojibake: {}",
            recorded
        );
        assert!(
            recorded["response_headers"]
                .get("content-encoding")
                .is_none(),
            "a replay must not claim gzip encoding on bytes that are already decoded: {}",
            recorded
        );
    }
}
