use crate::types::{create_mock_key, MockStore};
use axum::{
    extract::State,
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
};
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
) -> Response {
    let path = uri.path();
    let method_str = method.as_str();

    debug!("Incoming request: {} {}", method_str, path);

    // Create lookup key
    let key = create_mock_key(method_str, path);

    // Try to find matching mock
    match state.mocks.get(&key) {
        Some(mock) => {
            info!(
                "Mock matched: {} {} -> {}",
                method_str, path, mock.status
            );

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
            },
        );

        mocks.insert(
            "POST:/login".to_string(),
            MockConfig {
                method: "POST".to_string(),
                path: "/login".to_string(),
                status: 201,
                response: json!({"token": "test-token"}),
            },
        );

        AppState {
            mocks: Arc::new(mocks),
        }
    }

    #[tokio::test]
    async fn test_handle_request_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/users".parse().unwrap();

        let response = handle_request(method, uri, State(state)).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_request_not_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/nonexistent".parse().unwrap();

        let response = handle_request(method, uri, State(state)).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = create_test_state();
        let response = health_check(State(state)).await;

        assert_eq!(response.0["status"], "healthy");
        assert_eq!(response.0["mocks_loaded"], 2);
    }
}
