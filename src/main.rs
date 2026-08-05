mod handler;
mod loader;
mod matcher;
mod regex_cache;
mod template;
mod types;

use axum::{
    routing::{any, get, post},
    Router,
};
use handler::{
    admin_dashboard, clear_requests, handle_request, health_check, list_requests, max_body_size,
    reset_sequences, AppState,
};
use loader::{load_mocks, load_mocks_map};
use std::env;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
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
    info!("  Max request body: {} bytes", max_body_size());

    // Load mock configurations
    let mocks = load_mocks(MOCKS_DIR);
    {
        let m = mocks.read().await;
        info!("Loaded {} mock(s)", m.len());
    }

    // Create application state
    let state = AppState::new(mocks.clone());

    // Spawn background task for hot-reloading mock files
    const RELOAD_INTERVAL_SECS: u64 = 2;
    let reload_mocks = mocks;
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(RELOAD_INTERVAL_SECS));
        // tokio::time::interval fires immediately on creation; skip the first
        // tick to avoid redundantly reloading mocks that were just loaded at startup.
        interval.tick().await;
        loop {
            interval.tick().await;
            let mocks_dir = MOCKS_DIR;
            // Run blocking file I/O off the async runtime to avoid stalling request handling
            let result = tokio::task::spawn_blocking(move || load_mocks_map(mocks_dir))
                .await
                .unwrap_or_else(|e| {
                    warn!("Hot reload task panicked: {}", e);
                    loader::LoadResult {
                        mocks: std::collections::HashMap::new(),
                        errors: 1,
                    }
                });
            if result.errors > 0 {
                warn!(
                    "🔄 Hot reload: {} error(s) loading mocks, keeping previous mock set",
                    result.errors
                );
                continue;
            }
            let mut store = reload_mocks.write().await;
            let old_len = store.len();
            *store = result.mocks;
            let new_len = store.len();
            if old_len != new_len {
                info!(
                    "🔄 Hot reload: mocks updated ({} -> {} endpoint(s))",
                    old_len, new_len
                );
            } else {
                debug!(
                    "🔄 Hot reload: mocks reloaded ({} endpoint(s), no changes)",
                    new_len
                );
            }
        }
    });

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
    info!("🔄 Hot reload enabled (checking every 2s)");

    // Start server
    axum::serve(listener, app).await.unwrap_or_else(|e| {
        panic!("Server error: {}", e);
    });
}

fn create_router(state: AppState) -> Router {
    Router::new()
        // Health check endpoint
        .route("/health", get(health_check))
        // Admin dashboard and request history API
        .route("/admin/dashboard", get(admin_dashboard))
        .route("/admin/requests", get(list_requests).delete(clear_requests))
        .route("/admin/sequences/reset", post(reset_sequences))
        // Catch-all route for mock requests
        .fallback(any(handle_request))
        // Add state
        .with_state(state)
        // Add tracing middleware
        //
        // The request-body cap is enforced in `handle_request` rather than by
        // a `tower_http` `RequestBodyLimitLayer` here: the layer's rejection
        // carries a plain-text body, which would answer the same condition two
        // different ways depending on whether the client declared a
        // Content-Length. `Limited` in the handler caps the stream just as
        // hard and always returns the documented JSON shape.
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

    fn test_state() -> AppState {
        AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())))
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = create_router(test_state());

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
        let app = create_router(test_state());

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
        let app = create_router(test_state());

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
        let app = create_router(test_state());

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

    #[tokio::test]
    async fn test_admin_dashboard_endpoint() {
        let app = create_router(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body_str.contains("Mimic"));
        assert!(body_str.contains("/admin/requests"));
    }

    #[tokio::test]
    async fn test_admin_sequences_reset_endpoint() {
        let app = create_router(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/sequences/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["reset"], 0);
    }

    #[tokio::test]
    async fn test_admin_sequences_reset_with_path() {
        let app = create_router(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/sequences/reset?path=/api/submit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["reset"], 0);
    }

    // ------------------------------------------------------------------
    // Body limit through the full router (#50)
    // ------------------------------------------------------------------

    /// A Content-Length-less body that keeps producing chunks, so the limit
    /// has to be enforced mid-stream rather than from a declared size.
    struct ChunkedBody {
        remaining: usize,
    }

    impl http_body::Body for ChunkedBody {
        type Data = bytes::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, Self::Error>>> {
            if self.remaining == 0 {
                return std::task::Poll::Ready(None);
            }
            let chunk = 64 * 1024;
            self.remaining = self.remaining.saturating_sub(chunk);
            std::task::Poll::Ready(Some(Ok(http_body::Frame::data(bytes::Bytes::from(vec![
                    b'x';
                    chunk
                ])))))
        }
    }

    /// Both shapes must produce the same documented JSON body, not one JSON
    /// and one plain-text rejection.
    async fn assert_413_json(body: Body) {
        let app = create_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("content-type", "text/plain")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "payload too large");
        assert_eq!(json["max_body_size"], handler::max_body_size());
    }

    #[tokio::test]
    async fn test_router_rejects_oversized_sized_body_with_413() {
        assert_413_json(Body::from(vec![b'x'; handler::max_body_size() + 1])).await;
    }

    #[tokio::test]
    async fn test_router_rejects_oversized_chunked_body_with_413() {
        assert_413_json(Body::new(ChunkedBody {
            remaining: handler::max_body_size() * 2,
        }))
        .await;
    }
}
