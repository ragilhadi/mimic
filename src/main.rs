mod faker;
mod handler;
mod loader;
mod matcher;
mod openapi;
mod regex_cache;
mod template;
mod types;

use axum::{
    routing::{any, get, post},
    Router,
};
use handler::{
    admin_dashboard, clear_requests, configured_port, handle_request, health_check, list_mocks,
    list_requests, list_sequences, max_body_size, max_log_entries, reset_sequences, AppState,
};
use loader::{load_mocks, load_mocks_map};
use std::collections::HashMap;
use std::env;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Application entry point.
///
/// Dispatches the one-off `import-openapi` subcommand if it was asked for,
/// otherwise starts the mock server. The subcommand check happens before the
/// async runtime spins up: importing a spec is plain blocking file I/O and
/// has no business starting a server.
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("import-openapi") {
        std::process::exit(run_import(&args[2..]));
    }

    run_server();
}

/// Run the `import-openapi` subcommand, returning the process exit code.
fn run_import(args: &[String]) -> i32 {
    match openapi::run_import(args) {
        Ok(written) => {
            for path in &written {
                println!("{}", path.display());
            }
            println!("✅ Generated {} mock file(s)", written.len());
            0
        }
        Err(openapi::ImportError::Usage(message)) => {
            // An empty message means `--help`, which isn't an error.
            if message.is_empty() {
                print!("{}", openapi::USAGE);
                return 0;
            }
            eprintln!("error: {}", message);
            eprint!("\n{}", openapi::USAGE);
            2
        }
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

/// Initializes logging, loads mock configurations, sets up the HTTP server,
/// and starts listening for incoming requests.
#[tokio::main]
async fn run_server() {
    // Initialize tracing/logging
    init_logging();

    info!("🧩 Starting Mimic Mock API Server");

    // Hardcoded mocks directory - Docker volume mount handles the path mapping
    const MOCKS_DIR: &str = "/app/mocks";

    // Get port from environment variable
    let port = configured_port();

    info!("Configuration:");
    info!("  Mocks directory: {}", MOCKS_DIR);
    info!("  Port: {}", port);
    info!("  Max request body: {} bytes", max_body_size());
    match max_log_entries() {
        0 => info!("  Request log: unbounded"),
        n => info!("  Request log: last {} request(s)", n),
    }

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
            let mut store = reload_mocks.write().await;
            let old_len = store.len();
            *store = apply_reload(&store, result);
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

/// Build the mock map a hot-reload cycle should install, given what's
/// currently registered and what the cycle managed to load.
///
/// A clean cycle replaces the map outright. A cycle with per-file errors —
/// one file mid-save, one teammate's typo — applies every mock that *did*
/// parse, so unrelated edits still take effect, and carries forward any route
/// the previous map had that this cycle didn't produce. That last part keeps
/// the broken file's own route serving its last-known-good response instead of
/// disappearing and reappearing as the file is fixed.
///
/// `loader.rs` has already logged which files failed and why.
fn apply_reload(
    current: &HashMap<String, Vec<types::MockConfig>>,
    result: loader::LoadResult,
) -> HashMap<String, Vec<types::MockConfig>> {
    if result.errors == 0 {
        return result.mocks;
    }

    let mut merged = result.mocks;
    let mut retained = 0usize;
    for (key, mocks) in current {
        if !merged.contains_key(key) {
            merged.insert(key.clone(), mocks.clone());
            retained += 1;
        }
    }

    warn!(
        "🔄 Hot reload: {} file(s) failed to load; applied {} endpoint(s) that parsed, \
         kept {} previously-registered endpoint(s)",
        result.errors,
        merged.len() - retained,
        retained
    );

    merged
}

fn create_router(state: AppState) -> Router {
    Router::new()
        // Health check endpoint
        .route("/health", get(health_check))
        // Admin dashboard and request history API
        .route("/admin/dashboard", get(admin_dashboard))
        .route("/admin/requests", get(list_requests).delete(clear_requests))
        .route("/admin/mocks", get(list_mocks))
        .route("/admin/sequences", get(list_sequences))
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

    // ------------------------------------------------------------------
    // Hot-reload failure isolation (#57)
    // ------------------------------------------------------------------

    use loader::LoadResult;
    use types::MockConfig;

    fn mock(method: &str, path: &str, marker: &str) -> MockConfig {
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
        }
    }

    fn map(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
        let mut out: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for m in mocks {
            out.entry(types::create_mock_key(&m.method, &m.path))
                .or_default()
                .push(m);
        }
        out
    }

    fn write_mock(dir: &std::path::Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn test_clean_reload_replaces_the_map() {
        let current = map(vec![mock("GET", "/old", "old")]);
        let result = LoadResult {
            mocks: map(vec![mock("GET", "/new", "new")]),
            errors: 0,
        };

        let next = apply_reload(&current, result);

        assert!(next.contains_key("GET:/new"));
        assert!(
            !next.contains_key("GET:/old"),
            "a clean reload must not resurrect deleted routes"
        );
    }

    #[test]
    fn test_broken_file_does_not_block_unrelated_changes() {
        // /a was already registered; /b is new and parsed fine this cycle;
        // some third file failed to parse.
        let current = map(vec![mock("GET", "/a", "a")]);
        let result = LoadResult {
            mocks: map(vec![mock("GET", "/a", "a"), mock("GET", "/b", "b")]),
            errors: 1,
        };

        let next = apply_reload(&current, result);

        assert!(
            next.contains_key("GET:/b"),
            "a valid file's route must register even when another file is broken"
        );
        assert_eq!(next["GET:/a"][0].response["source"], "a");
    }

    #[test]
    fn test_broken_files_route_keeps_its_last_good_response() {
        // /broken parsed last cycle but not this one. Its route must keep
        // serving rather than flap out of existence while the file is fixed.
        let current = map(vec![
            mock("GET", "/broken", "last-good"),
            mock("GET", "/ok", "ok"),
        ]);
        let result = LoadResult {
            mocks: map(vec![mock("GET", "/ok", "ok-updated")]),
            errors: 1,
        };

        let next = apply_reload(&current, result);

        assert_eq!(next["GET:/broken"][0].response["source"], "last-good");
        assert_eq!(next["GET:/ok"][0].response["source"], "ok-updated");
    }

    #[test]
    fn test_reload_from_disk_with_one_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        write_mock(
            dir.path(),
            "a.json",
            r#"{"method":"GET","path":"/a","status":200,"response":{}}"#,
        );
        let current = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(current.errors, 0);
        assert!(current.mocks.contains_key("GET:/a"));

        // A second valid mock lands at the same time as a file mid-save.
        write_mock(
            dir.path(),
            "b.json",
            r#"{"method":"GET","path":"/b","status":200,"response":{}}"#,
        );
        write_mock(dir.path(), "c.json", "{invalid json");

        let result = load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(result.errors, 1);

        let next = apply_reload(&current.mocks, result);

        assert!(next.contains_key("GET:/a"));
        assert!(
            next.contains_key("GET:/b"),
            "/b is independently valid and must become available"
        );
    }
}
