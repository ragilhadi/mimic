mod handler;
mod loader;
mod matcher;
mod types;

use axum::{
    routing::{any, get},
    Router,
};
use handler::{clear_requests, handle_request, health_check, list_requests, AppState};
use loader::load_mocks;
use std::env;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Application entry point.
///
/// Initializes logging, loads mock configurations, sets up the HTTP server,
/// and starts listening for incoming requests.
#[tokio::main]
async fn main() {
    // Initialize tracing/logging
    init_logging();

    info!("🧩 Starting Mimic Mock API Server");

    // Hardcoded mocks directory - Docker volume mount handles the path mapping
    const MOCKS_DIR: &str = "/app/mocks";

    // Get port from environment variable
    let port = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .unwrap_or(8080);

    info!("Configuration:");
    info!("  Mocks directory: {}", MOCKS_DIR);
    info!("  Port: {}", port);

    // Load mock configurations
    let mocks = load_mocks(MOCKS_DIR);
    info!("Loaded {} mock(s)", mocks.len());

    // Create application state
    let state = AppState {
        mocks,
        request_log: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
    };

    // Build router
    let app = create_router(state);

    // Create server address
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            panic!("Failed to bind to {}: {}", addr, e);
        });

    info!("🚀 Server listening on http://{}", addr);
    info!("📋 Health check available at http://{}/health", addr);

    // Start server
    axum::serve(listener, app).await.unwrap_or_else(|e| {
        panic!("Server error: {}", e);
    });
}

fn create_router(state: AppState) -> Router {
    Router::new()
        // Health check endpoint
        .route("/health", get(health_check))
        // Request history / inspection API
        .route("/admin/requests", get(list_requests).delete(clear_requests))
        // Catch-all route for mock requests
        .fallback(any(handle_request))
        // Add state
        .with_state(state)
        // Add tracing middleware
        .layer(TraceLayer::new_for_http())
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,mimic=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_thread_ids(false)
                .with_line_number(false),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = AppState {
            mocks: Arc::new(HashMap::new()),
            request_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        };
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_fallback_route() {
        let state = AppState {
            mocks: Arc::new(HashMap::new()),
            request_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        };
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/undefined")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should return 404 from mock handler
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_admin_requests_endpoint() {
        let state = AppState {
            mocks: Arc::new(HashMap::new()),
            request_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        };
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/requests")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["count"], 0);
    }

    #[tokio::test]
    async fn test_admin_requests_delete() {
        let state = AppState {
            mocks: Arc::new(HashMap::new()),
            request_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        };
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/admin/requests")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
