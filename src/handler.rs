use crate::types::{create_mock_key, MockStore};
use axum::{
    body::Body,
    extract::State,
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
};
use http_body_util::BodyExt;
use serde_json::json;
use tracing::{debug, info};

#[derive(Clone)]
pub struct AppState {
    pub mocks: MockStore,
}

pub async fn handle_request(
    method: Method,
    uri: Uri,
    State(state): State<AppState>,
    body: Body,
) -> Response {
    let path = uri.path();
    let method_str = method.as_str();

    debug!("Incoming request: {} {}", method_str, path);

    // Create lookup key first to check if we should consume body
    let key = create_mock_key(method_str, path);

    // Check if we should consume the request body based on mock configuration
    let should_consume = state
        .mocks
        .get(&key)
        .map(|mock| mock.consume_body)
        .unwrap_or(false); // Default to false if no mock found (fast by default)

    // Consume request body to prevent broken pipe errors
    // Critical for POST/PUT/PATCH with large payloads (e.g., multipart/form-data with images)
    if should_consume && matches!(method, Method::POST | Method::PUT | Method::PATCH) {
        if let Ok(collected) = body.collect().await {
            let bytes = collected.to_bytes();
            debug!("Consumed {} bytes from request body", bytes.len());
        }
    }

    // Try to find matching mock
    match state.mocks.get(&key) {
        Some(mock) => {
            info!("Mock matched: {} {} -> {}", method_str, path, mock.status);

            // Convert status code
            let status = StatusCode::from_u16(mock.status).unwrap_or(StatusCode::OK);

            // Return configured response
            (status, Json(mock.response.clone())).into_response()
        }
        None => {
            info!("No mock found for: {} {}", method_str, path);

            // Return 404 with error message
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "mock not found",
                    "method": method_str,
                    "path": path
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
    use crate::types::MockConfig;
    use axum::http::Method;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn create_test_state() -> AppState {
        let mut mocks = HashMap::new();

        mocks.insert(
            "GET:/users".to_string(),
            MockConfig {
                method: "GET".to_string(),
                path: "/users".to_string(),
                status: 200,
                response: json!({"users": [{"id": 1, "name": "Alice"}]}),
                consume_body: true,
            },
        );

        mocks.insert(
            "POST:/login".to_string(),
            MockConfig {
                method: "POST".to_string(),
                path: "/login".to_string(),
                status: 201,
                response: json!({"token": "test-token"}),
                consume_body: true,
            },
        );

        mocks.insert(
            "DELETE:/users/123".to_string(),
            MockConfig {
                method: "DELETE".to_string(),
                path: "/users/123".to_string(),
                status: 204,
                response: json!(null),
                consume_body: true,
            },
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

        let response = handle_request(method, uri, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_request_not_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/nonexistent".parse().unwrap();

        let response = handle_request(method, uri, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_post_method() {
        let state = create_test_state();
        let method = Method::POST;
        let uri = "/login".parse().unwrap();

        let response = handle_request(method, uri, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_handle_request_delete_method() {
        let state = create_test_state();
        let method = Method::DELETE;
        let uri = "/users/123".parse().unwrap();

        let response = handle_request(method, uri, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_handle_request_wrong_method() {
        let state = create_test_state();
        let method = Method::PUT;
        let uri = "/users".parse().unwrap();

        let response = handle_request(method, uri, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_case_sensitive_path() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/Users".parse().unwrap(); // Different case

        let response = handle_request(method, uri, State(state), Body::empty()).await;

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

        let response = handle_request(method, uri, State(state), Body::empty()).await;

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

        let response = handle_request(method, uri, State(state), Body::empty()).await;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

        assert_eq!(parts.status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "mock not found");
        assert_eq!(json["method"], "GET");
        assert_eq!(json["path"], "/nonexistent");
    }

    #[tokio::test]
    async fn test_handle_request_post_with_large_body() {
        let state = create_test_state();
        let method = Method::POST;
        let uri = "/login".parse().unwrap();

        // Create a large body (1 MB) to simulate image upload
        let large_body = vec![0u8; 1024 * 1024];
        let body = Body::from(large_body);

        let response = handle_request(method, uri, State(state), body).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_handle_request_post_with_multipart_body() {
        let state = create_test_state();
        let method = Method::POST;
        let uri = "/login".parse().unwrap();

        // Simulate multipart/form-data with image content
        let body_content = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n<binary image data>\r\n--boundary--";
        let body = Body::from(body_content.to_vec());

        let response = handle_request(method, uri, State(state), body).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_handle_request_put_with_body() {
        let state = create_test_state();
        let method = Method::PUT;
        let uri = "/users/1".parse().unwrap();

        let body_content = b"{\"name\": \"Updated User\"}";
        let body = Body::from(body_content.to_vec());

        let response = handle_request(method, uri, State(state), body).await;

        // Should return 404 as we don't have a mock for PUT /users/1
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_patch_with_body() {
        let state = create_test_state();
        let method = Method::PATCH;
        let uri = "/users/1".parse().unwrap();

        let body_content = b"{\"status\": \"active\"}";
        let body = Body::from(body_content.to_vec());

        let response = handle_request(method, uri, State(state), body).await;

        // Should return 404 as we don't have a mock for PATCH /users/1
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_with_consume_body_true() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/upload".to_string(),
            MockConfig {
                method: "POST".to_string(),
                path: "/upload".to_string(),
                status: 200,
                response: json!({"uploaded": true}),
                consume_body: true, // Explicitly enable body consumption
            },
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

        let method = Method::POST;
        let uri = "/upload".parse().unwrap();
        let body_content = vec![0u8; 1024 * 100]; // 100 KB
        let body = Body::from(body_content);

        let response = handle_request(method, uri, State(state), body).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_request_with_consume_body_false() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/no-consume".to_string(),
            MockConfig {
                method: "POST".to_string(),
                path: "/no-consume".to_string(),
                status: 200,
                response: json!({"message": "ok"}),
                consume_body: false, // Explicitly disable body consumption
            },
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

        let method = Method::POST;
        let uri = "/no-consume".parse().unwrap();
        let body_content = b"some data";
        let body = Body::from(body_content.to_vec());

        let response = handle_request(method, uri, State(state), body).await;

        // Should still return OK even though body is not consumed
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_request_default_consume_body() {
        // Test that consume_body defaults to false when not specified (fast by default)
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/default".to_string(),
            MockConfig {
                method: "POST".to_string(),
                path: "/default".to_string(),
                status: 201,
                response: json!({"created": true}),
                consume_body: false, // Default value (fast)
            },
        );

        let state = AppState {
            mocks: Arc::new(mocks),
        };

        let method = Method::POST;
        let uri = "/default".parse().unwrap();
        let body_content = b"small body";
        let body = Body::from(body_content.to_vec());

        let response = handle_request(method, uri, State(state), body).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }
}
