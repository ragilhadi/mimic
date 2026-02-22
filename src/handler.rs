use crate::matcher::{find_matching_mock, parse_headers, parse_query_string, RequestContext};
use crate::types::MockStore;
use axum::{
    body::Body,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::json;
use tracing::{debug, info};

#[derive(Clone)]
pub struct AppState {
    pub mocks: MockStore,
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

    // Check if any mock needs body matching
    let needs_body_matching = state.mocks.values().flatten().any(|mock| mock.body.is_some());

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
    let context = RequestContext {
        method: method_str.clone(),
        path: path.clone(),
        query_params,
        headers: parsed_headers.clone(),
        body: body_bytes,
        content_type,
    };

    // Find matching mock using the new matcher
    match find_matching_mock(&context, &state.mocks) {
        Some(mock) => {
            info!("Mock matched: {} {} -> {}", method_str, path, mock.status);

            // Convert status code
            let status = StatusCode::from_u16(mock.status).unwrap_or(StatusCode::OK);

            // Return configured response
            (status, Json(mock.response.clone())).into_response()
        }
        None => {
            info!("No mock found for: {} {}", method_str, path);

            // Return 404 with detailed error message
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "mock not found",
                    "method": method_str,
                    "path": path,
                    "query_params": context.query_params,
                    "headers_received": parsed_headers.keys().collect::<Vec<_>>()
                })),
            )
                .into_response()
        }
    }
}

pub async fn health_check(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "healthy",
        "mocks_loaded": state.mocks.len(),
        "service": "mimic"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BodyMatcher, HeaderMatcher, HeaderPattern, HeaderValue, JsonBodyMatcher, MockConfig,
        QueryParamMatcher, QueryParamPattern, QueryParamValue,
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
            }],
        );

        AppState {
            mocks: Arc::new(mocks),
        }
    }

    fn create_empty_state() -> AppState {
        AppState {
            mocks: Arc::new(HashMap::new()),
        }
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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
            }],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

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
                },
            ],
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

        // Request with admin role should return admin mock
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"role":"admin"}"#);
        let response =
            handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
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
}
