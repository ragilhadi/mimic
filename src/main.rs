mod cors;
mod faker;
mod handler;
mod loader;
mod matcher;
mod openapi;
mod proxy;
mod regex_cache;
mod template;
mod types;

use axum::{
    routing::{any, get, post},
    Router,
};
use handler::{
    admin_dashboard, clear_requests, configured_active_tags, configured_port, get_scenario,
    handle_request, health_check, list_mocks, list_requests, list_sequences, max_body_size,
    max_log_entries, reset_sequences, set_scenario, AppState,
};
use loader::{load_mocks, load_mocks_map};
use std::collections::{HashMap, HashSet};
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
            if let Some(out_dir) = written.first().and_then(|p| p.parent()) {
                print_serve_hint(out_dir);
            }
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

/// Tell the operator how to make the server serve what the importer just
/// wrote.
///
/// The default output directory sits under `./mocks`, which the server reads
/// by default on a local run — but not when it resolves the Docker mount or a
/// directory named by `MIMIC_MOCKS_DIR`, and not when `--out` pointed
/// somewhere else entirely. Rather than assume, this prints whichever of the
/// two statements is true right now.
fn print_serve_hint(out_dir: &std::path::Path) {
    let resolved = loader::resolve_mocks_dir();
    let already_served = std::path::Path::new(&resolved.path)
        .canonicalize()
        .ok()
        .zip(out_dir.canonicalize().ok())
        .is_some_and(|(root, out)| out.starts_with(root));

    if already_served {
        println!(
            "   {} is already the directory Mimic reads — start it with: mimic",
            resolved.path
        );
    } else {
        println!(
            "   Serve them with: {}={} mimic",
            loader::MOCKS_DIR_ENV,
            out_dir.display()
        );
    }
}

/// Initializes logging, loads mock configurations, sets up the HTTP server,
/// and starts listening for incoming requests.
#[tokio::main]
async fn run_server() {
    // Initialize tracing/logging
    init_logging();

    info!("🧩 Starting Mimic Mock API Server");

    // Where mocks are read from: MIMIC_MOCKS_DIR, else /app/mocks when it
    // exists (the Docker image), else ./mocks.
    let mocks_dir = loader::resolve_mocks_dir();

    // Get port from environment variable
    let port = configured_port();

    // Scenario selection: which tagged mock groups start out matchable
    let active_tags = configured_active_tags();

    // Which method+path pairs Mimic answers itself, and therefore can't serve
    // from a mock.
    let reserved = handler::ReservedRoutes::from_env();

    // What is scrubbed from the bodies the request log keeps, and whether the
    // admin API that serves them is behind a token.
    let redaction = handler::BodyRedaction::from_env();
    let admin_token = handler::configured_admin_token();

    info!("Configuration:");
    info!(
        "  Mocks directory: {} ({})",
        mocks_dir.path,
        mocks_dir.origin.describe()
    );
    info!("  Port: {}", port);
    info!("  Max request body: {} bytes", max_body_size());
    match active_tags.as_ref() {
        Some(tags) => {
            let mut names: Vec<&str> = tags.iter().map(String::as_str).collect();
            names.sort();
            info!("  Active scenario tags: {}", names.join(", "));
        }
        None => info!("  Active scenario tags: (none — all mocks matchable)"),
    }
    match max_log_entries() {
        0 => info!("  Request log: unbounded"),
        n => info!("  Request log: last {} request(s)", n),
    }
    info!(
        "  Health check: {}",
        reserved.health.as_deref().unwrap_or("(disabled)")
    );
    info!(
        "  Admin API: {}{}",
        reserved
            .admin_prefix
            .as_deref()
            .map_or("(disabled)".to_string(), |prefix| format!("{}/*", prefix)),
        if admin_token.is_some() {
            " (bearer token required)"
        } else {
            " (unauthenticated)"
        }
    );
    if redaction.disabled {
        info!(
            "  Stored bodies: none ({}=true)",
            handler::DISABLE_BODY_LOG_ENV
        );
    } else if redaction.fields.is_empty() {
        info!(
            "  Stored bodies: verbatim, no field redaction ({}= is empty)",
            handler::REDACT_BODY_FIELDS_ENV
        );
    } else {
        let mut fields: Vec<&str> = redaction.fields.iter().map(String::as_str).collect();
        fields.sort();
        info!("  Redacted body fields: {}", fields.join(", "));
    }
    // Logged at startup because a CORS setup that isn't taking effect is
    // otherwise diagnosed from the browser console, which only ever says the
    // header was missing.
    match cors::configured() {
        Some(config) => {
            let origins = match &config.origins {
                cors::OriginRule::Any => "*".to_string(),
                cors::OriginRule::List(list) => list.join(", "),
            };
            info!(
                "  CORS: enabled (origins: {}; methods: {}; credentials: {}; max-age: {}s)",
                origins,
                config.methods.join(", "),
                config.credentials,
                config.max_age
            );
        }
        None => info!("  CORS: disabled (set MIMIC_CORS=true to enable)"),
    }

    // Proxy/passthrough: forward a request that matches no local mock to a
    // real upstream instead of 404ing, optionally recording it as a new mock.
    let proxy_config = proxy::configured_proxy_config(port).map(std::sync::Arc::new);
    match proxy_config.as_ref() {
        Some(cfg) => info!(
            "  Proxy upstream: {} (record={}, timeout={}ms)",
            cfg.upstream, cfg.record, cfg.timeout_ms
        ),
        None => info!("  Proxy upstream: (none — unmatched requests return 404)"),
    }

    // Load mock configurations
    let mocks = load_mocks(&mocks_dir.path);
    let mut shadowed = {
        let m = mocks.read().await;
        info!("Loaded {} mock(s)", m.len());
        if m.is_empty() {
            warn_empty_mock_set(&mocks_dir);
        }
        let shadowed = shadowed_mocks(&m, &reserved);
        warn_about_shadowed_mocks(&shadowed);
        shadowed
    };

    // Create application state
    let state = AppState::with_active_tags(mocks.clone(), active_tags)
        .with_reserved(reserved)
        .with_redaction(redaction)
        .with_admin_token(admin_token)
        .with_mocks_dir(mocks_dir.path.clone())
        .with_proxy_config(proxy_config);

    // Spawn background task for hot-reloading mock files
    const RELOAD_INTERVAL_SECS: u64 = 2;
    let reload_mocks = mocks;
    // The reloader watches the directory that was actually resolved, not a
    // second copy of the resolution rule that could disagree with it.
    let reload_dir = mocks_dir.path.clone();
    // Per-mock state the reloader has to reconcile against the new mock set.
    let reload_counters = state.sequence_counters.clone();
    let reload_hits = state.mock_hits.clone();
    let reload_reserved = state.reserved.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(RELOAD_INTERVAL_SECS));
        // tokio::time::interval fires immediately on creation; skip the first
        // tick to avoid redundantly reloading mocks that were just loaded at startup.
        interval.tick().await;
        loop {
            interval.tick().await;
            let mocks_dir = reload_dir.clone();
            // Run blocking file I/O off the async runtime to avoid stalling request handling
            let result = tokio::task::spawn_blocking(move || load_mocks_map(&mocks_dir))
                .await
                .unwrap_or_else(|e| {
                    warn!("Hot reload task panicked: {}", e);
                    loader::LoadResult {
                        mocks: std::collections::HashMap::new(),
                        errors: 1,
                    }
                });
            // The mock lock is released before the counter locks are taken:
            // `list_sequences` reads counters and then mocks, so holding both
            // in the opposite order here would be a deadlock waiting to happen.
            let live = {
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
                // Warn only when the set changes: this loop runs every two
                // seconds, and a collision that has already been reported does
                // not need reporting 30 times a minute.
                let current = shadowed_mocks(&store, &reload_reserved);
                if current != shadowed {
                    warn_about_shadowed_mocks(&current);
                    shadowed = current;
                }
                live_identities(&store)
            };

            prune_stale_state(&reload_counters, &reload_hits, &live).await;
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

/// One mock that will never serve a request because Mimic answers its
/// method+path itself: the file it came from, and what claims the route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ShadowedMock {
    method: String,
    path: String,
    source: String,
    owner: &'static str,
}

/// Every loaded mock a reserved route shadows, sorted so the set can be
/// compared between reloads.
fn shadowed_mocks(
    store: &HashMap<String, Vec<types::MockConfig>>,
    reserved: &handler::ReservedRoutes,
) -> Vec<ShadowedMock> {
    let mut found: Vec<ShadowedMock> = store
        .values()
        .flatten()
        .filter_map(|mock| {
            reserved
                .reservation_for(&mock.method, &mock.path)
                .map(|owner| ShadowedMock {
                    method: mock.method.clone(),
                    path: mock.path.clone(),
                    source: mock
                        .source
                        .clone()
                        .unwrap_or_else(|| "(built in-process)".to_string()),
                    owner,
                })
        })
        .collect();
    found.sort();
    found
}

/// Say out loud that a mock is loaded and unreachable.
///
/// Explicit routes are matched ahead of the fallback by design, so such a mock
/// parses, registers, is listed by `/admin/mocks`, and then sits at `hits: 0`
/// forever. Nothing else in the system can report it: the request never
/// reaches `handle_request`, so it is never recorded, and the dashboard —
/// whose whole premise is reading *why* a request didn't match — is blind to
/// precisely this one.
fn warn_about_shadowed_mocks(shadowed: &[ShadowedMock]) {
    for mock in shadowed {
        warn!(
            "{} declares {} {}, which is reserved by {} and will never be served. \
             Move it, or free the route (see MIMIC_HEALTH_PATH / MIMIC_ADMIN_PREFIX / \
             MIMIC_DISABLE_ADMIN).",
            mock.source, mock.method, mock.path, mock.owner
        );
    }
}

/// Every mock identity a mock map currently registers.
fn live_identities(
    store: &HashMap<String, Vec<types::MockConfig>>,
) -> HashSet<types::MockIdentity> {
    store
        .iter()
        .flat_map(|(key, list)| {
            list.iter()
                .enumerate()
                .map(move |(index, mock)| types::MockIdentity::of(mock, key, index))
        })
        .collect()
}

/// Drop per-mock state belonging to mocks the latest reload no longer loads.
///
/// A counter for a deleted mock used to be left behind indefinitely: harmless
/// on its own, except that its key was a *position*, so the next mock to land
/// in that slot inherited a half-consumed sequence and someone else's hit
/// count. Identities are stable now, which stops the misattribution — but an
/// orphan still has to go, or `/admin/sequences` accumulates rows for files
/// that no longer exist and `/admin/mocks` reports totals nothing can explain.
///
/// A mock that merely stops parsing keeps its state: `apply_reload` carries
/// its last-known-good route forward, so it's still registered and still live.
async fn prune_stale_state(
    counters: &types::SequenceCounters,
    hits: &handler::MockHits,
    live: &HashSet<types::MockIdentity>,
) {
    let dropped = {
        let mut counters = counters.write().await;
        let before = counters.len();
        counters.retain(|identity, _| live.contains(identity));
        before - counters.len()
    };

    {
        let mut hits = hits.write().await;
        hits.retain(|identity, _| live.contains(identity));
    }

    if dropped > 0 {
        info!(
            "🔄 Hot reload: dropped {} sequence counter(s) for mocks that are no longer loaded",
            dropped
        );
    }
}

/// Explain a startup that registered no mocks at all.
///
/// A server answering `404 mock not found` for everything is indistinguishable
/// from a server that loaded fine, so the two ways of getting there are named
/// separately: the directory isn't there, or it is there and holds no mock
/// files. Previously both produced the same near-silence.
fn warn_empty_mock_set(dir: &loader::ResolvedMocksDir) {
    if !dir.exists {
        warn!(
            "No mocks loaded: '{}' does not exist. Point Mimic at your mocks with {}=<dir>, \
             or create '{}' next to the working directory.",
            dir.path,
            loader::MOCKS_DIR_ENV,
            dir.path
        );
    } else {
        warn!(
            "No mocks loaded: '{}' exists but contains no readable *.json mock files \
             (subdirectories are searched too). Every request will answer 404 mock not found.",
            dir.path
        );
    }
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

/// Build the HTTP router.
///
/// Every built-in route carries `.fallback(handle_request)`: a method the
/// route doesn't declare — `POST /health`, `PUT /admin/requests` — used to get
/// a bare `405 Method Not Allowed` from axum's method router, a response shape
/// Mimic doesn't document and never records in the request log. Those requests
/// now reach the normal pipeline, so they can be mocked, and are answered with
/// the documented `mock not found` body when they aren't.
///
/// Which routes exist at all comes from `state.reserved`, so `MIMIC_HEALTH_PATH`
/// and `MIMIC_ADMIN_PREFIX` / `MIMIC_DISABLE_ADMIN` can hand these paths back
/// to an API that genuinely owns them.
fn create_router(state: AppState) -> Router {
    let reserved = state.reserved.clone();
    let mut router = Router::new();

    if let Some(health_path) = reserved.health.as_deref() {
        router = router.route(health_path, get(health_check).fallback(handle_request));
    }

    if let Some(prefix) = reserved.admin_prefix.as_deref() {
        let at = |suffix: &str| format!("{}{}", prefix, suffix);
        router = router
            // Admin dashboard and request history API
            .route(
                &at("/dashboard"),
                get(admin_dashboard).fallback(handle_request),
            )
            .route(
                &at("/requests"),
                get(list_requests)
                    .delete(clear_requests)
                    .fallback(handle_request),
            )
            .route(&at("/mocks"), get(list_mocks).fallback(handle_request))
            .route(
                &at("/sequences"),
                get(list_sequences).fallback(handle_request),
            )
            .route(
                &at("/sequences/reset"),
                post(reset_sequences).fallback(handle_request),
            )
            // Scenario switching: which tagged mock groups are currently matchable
            .route(
                &at("/scenario"),
                get(get_scenario)
                    .post(set_scenario)
                    .fallback(handle_request),
            );
    }

    let app = router
        // Catch-all route for mock requests
        .fallback(any(handle_request))
        // Add state
        .with_state(state.clone())
        // Add tracing middleware
        //
        // The request-body cap is enforced in `handle_request` rather than by
        // a `tower_http` `RequestBodyLimitLayer` here: the layer's rejection
        // carries a plain-text body, which would answer the same condition two
        // different ways depending on whether the client declared a
        // Content-Length. `Limited` in the handler caps the stream just as
        // hard and always returns the documented JSON shape.
        .layer(TraceLayer::new_for_http());

    // The admin token guard is attached only when a token is configured, so a
    // default run doesn't pay a per-request check for a feature it isn't
    // using. The middleware itself decides which requests it applies to, from
    // the same `ReservedRoutes` the routes above were registered from.
    if state.admin_token.is_some() {
        app.layer(axum::middleware::from_fn_with_state(
            state,
            handler::require_admin_token,
        ))
    } else {
        app
    }
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
    async fn test_admin_scenario_endpoint_is_routed() {
        let app = create_router(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/scenario")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["filtering"], false);
    }

    /// `curl -X POST /admin/scenario -d '{"tags": [...]}'` sends a form
    /// content type. The endpoint reads the raw body precisely so that
    /// documented one-liner works instead of 415-ing.
    #[tokio::test]
    async fn test_admin_scenario_post_accepts_a_body_without_a_json_content_type() {
        let state = test_state();
        let app = create_router(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/scenario")
                    .body(Body::from(r#"{"tags": ["error-scenario"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["active_tags"], serde_json::json!(["error-scenario"]));

        let active = state.active_tags.read().await;
        assert_eq!(
            active.as_ref().map(|tags| tags.len()),
            Some(1),
            "the POST should have taken effect on the shared state"
        );
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
    // Reserved routes (#87)
    // ------------------------------------------------------------------

    fn state_serving(mocks: Vec<MockConfig>) -> AppState {
        AppState::new(Arc::new(tokio::sync::RwLock::new(map(mocks))))
    }

    async fn call(app: Router, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn test_unsupported_method_on_a_reserved_path_is_not_a_bare_405() {
        // axum's method router answered `POST /health` with an empty 405 —
        // a shape Mimic doesn't document and never records.
        let app = create_router(test_state());
        let (status, body) = call(app, "POST", "/health").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "mock not found");
        assert_eq!(json["path"], "/health");
    }

    #[tokio::test]
    async fn test_a_method_the_admin_api_does_not_declare_reaches_the_mock_handler() {
        let app = create_router(state_serving(vec![mock("PUT", "/admin/requests", "mine")]));
        let (status, body) = call(app, "PUT", "/admin/requests").await;

        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source"], "mine");
    }

    #[tokio::test]
    async fn test_a_mock_cannot_shadow_the_health_check_by_default() {
        // The behavior this issue documents rather than changes: /health stays
        // Mimic's until someone frees it. The loader warning and the
        // `reachable: false` flag on /admin/mocks are what make it visible.
        let app = create_router(state_serving(vec![mock("GET", "/health", "mine")]));
        let (status, body) = call(app, "GET", "/health").await;

        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "healthy");
    }

    #[tokio::test]
    async fn test_freeing_the_health_path_lets_a_mock_serve_it() {
        let state = state_serving(vec![mock("GET", "/health", "mine")]).with_reserved(
            handler::ReservedRoutes {
                health: None,
                admin_prefix: Some("/admin".to_string()),
            },
        );
        let (status, body) = call(create_router(state), "GET", "/health").await;

        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source"], "mine", "the mock should win now");
    }

    #[tokio::test]
    async fn test_moving_the_admin_prefix_moves_every_admin_route() {
        let state = state_serving(vec![mock("GET", "/admin/mocks", "mine")]).with_reserved(
            handler::ReservedRoutes {
                health: Some("/health".to_string()),
                admin_prefix: Some("/_mimic".to_string()),
            },
        );
        let app = create_router(state.clone());

        let (status, body) = call(app, "GET", "/_mimic/mocks").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["mocks"].is_array(), "the admin API moved here");

        // …and the old prefix is now the mock set's to use.
        let (status, body) = call(create_router(state), "GET", "/admin/mocks").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source"], "mine");
    }

    #[tokio::test]
    async fn test_disabling_the_admin_api_removes_all_of_its_routes() {
        let state = state_serving(vec![]).with_reserved(handler::ReservedRoutes {
            health: Some("/health".to_string()),
            admin_prefix: None,
        });
        let (status, body) = call(create_router(state), "GET", "/admin/requests").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "mock not found");
    }

    // ------------------------------------------------------------------
    // Admin API authentication (#88)
    // ------------------------------------------------------------------

    async fn call_with_auth(
        app: Router,
        method: &str,
        uri: &str,
        auth: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(value) = auth {
            builder = builder.header("authorization", value);
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    fn token_state(mocks: Vec<MockConfig>) -> AppState {
        state_serving(mocks).with_admin_token(Some("s3cret".to_string()))
    }

    #[tokio::test]
    async fn test_admin_api_is_open_when_no_token_is_configured() {
        // The default has to stay exactly as it was, or every existing user's
        // dashboard stops working on upgrade.
        let (status, _) = call(create_router(test_state()), "GET", "/admin/requests").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_a_configured_token_is_required_on_the_admin_api() {
        let app = create_router(token_state(vec![]));
        let (status, body) = call_with_auth(app, "GET", "/admin/requests", None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "unauthorized");
    }

    #[tokio::test]
    async fn test_the_right_token_gets_through_and_a_wrong_one_does_not() {
        let (status, _) = call_with_auth(
            create_router(token_state(vec![])),
            "GET",
            "/admin/requests",
            Some("Bearer s3cret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        for wrong in ["Bearer nope", "s3cret", "Basic s3cret", "Bearer "] {
            let (status, _) = call_with_auth(
                create_router(token_state(vec![])),
                "GET",
                "/admin/requests",
                Some(wrong),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "'{}' must not be accepted",
                wrong
            );
        }
    }

    #[tokio::test]
    async fn test_the_token_does_not_guard_the_health_check() {
        // Liveness probes call /health and carry no credentials; requiring a
        // token there would fail every deployment that turned auth on.
        let (status, _) =
            call_with_auth(create_router(token_state(vec![])), "GET", "/health", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_the_token_does_not_guard_ordinary_mocks_under_the_admin_prefix() {
        // `GET /admin/users` is a mock path, not an admin endpoint — the same
        // distinction #87 drew for reserved routes.
        let app = create_router(token_state(vec![mock("GET", "/admin/users", "mine")]));
        let (status, body) = call_with_auth(app, "GET", "/admin/users", None).await;

        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source"], "mine");
    }

    #[test]
    fn test_shadowed_mocks_names_the_file_and_what_claims_the_route() {
        let mut health = mock("GET", "/health", "down");
        health.source = Some("mocks/health_down.json".to_string());
        let store = map(vec![health, mock("GET", "/users", "users")]);

        let found = shadowed_mocks(&store, &handler::ReservedRoutes::default());

        assert_eq!(found.len(), 1, "only the reserved path collides");
        assert_eq!(found[0].path, "/health");
        assert_eq!(found[0].source, "mocks/health_down.json");
    }

    #[test]
    fn test_nothing_is_shadowed_once_the_routes_are_freed() {
        let store = map(vec![
            mock("GET", "/health", "down"),
            mock("GET", "/admin/mocks", "mine"),
        ]);
        let freed = handler::ReservedRoutes {
            health: None,
            admin_prefix: None,
        };

        assert!(shadowed_mocks(&store, &freed).is_empty());
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
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
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

    // ------------------------------------------------------------------
    // Per-mock state survives a reload that reorders the bucket (#86)
    // ------------------------------------------------------------------

    /// A mock for `POST /orders` whose three steps announce which file they
    /// came from, so a step served by the wrong mock is visible in the body.
    fn sequenced_orders_mock(who: &str) -> String {
        let step = |n: u8| {
            format!(
                r#"{{"status":200,"response":{{"who":"{}","step":{}}}}}"#,
                who, n
            )
        };
        format!(
            r#"{{"method":"POST","path":"/orders","status":200,"response":{{}},
                 "sequence":[{},{},{}]}}"#,
            step(1),
            step(2),
            step(3)
        )
    }

    async fn post_orders(state: &AppState) -> serde_json::Value {
        let response = handle_request(
            axum::http::Method::POST,
            "/orders".parse().unwrap(),
            axum::http::HeaderMap::new(),
            axum::extract::State(state.clone()),
            Body::empty(),
        )
        .await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Run one hot-reload cycle against a directory, exactly as the background
    /// task does: reload, install, then reconcile per-mock state.
    async fn reload_cycle(state: &AppState, store: &types::MockStore, dir: &std::path::Path) {
        let result = load_mocks_map(dir.to_str().unwrap());
        let live = {
            let mut current = store.write().await;
            *current = apply_reload(&current, result);
            live_identities(&current)
        };
        prune_stale_state(&state.sequence_counters, &state.mock_hits, &live).await;
    }

    #[tokio::test]
    async fn test_adding_a_sibling_mock_does_not_move_an_existing_sequence() {
        // The issue's repro: a sequence half-consumed by one mock must not be
        // adopted by a file that lands ahead of it in the bucket.
        let dir = tempfile::tempdir().unwrap();
        write_mock(dir.path(), "b_orders.json", &sequenced_orders_mock("b"));

        let store = load_mocks(dir.path().to_str().unwrap());
        let state = AppState::new(store.clone());

        assert_eq!(post_orders(&state).await["step"], 1);
        assert_eq!(post_orders(&state).await["step"], 2);

        // A teammate adds a second scenario for the same endpoint. Its name
        // sorts first, so it takes bucket position 0 — the position the
        // half-consumed counter used to be keyed by.
        write_mock(dir.path(), "a_orders.json", &sequenced_orders_mock("a"));
        reload_cycle(&state, &store, dir.path()).await;

        // The new mock wins the tie-break and serves. It must start at its own
        // first step; keyed by position it would have inherited the other
        // mock's two calls and served step 3 on its first-ever request.
        let served = post_orders(&state).await;
        assert_eq!(served["who"], "a");
        assert_eq!(
            served["step"], 1,
            "a fresh mock must start its own sequence"
        );

        // And the original mock's position is untouched, not restarted.
        let counters = state.sequence_counters.read().await;
        let step_of = |file: &str| {
            counters
                .iter()
                .find(|(identity, _)| {
                    identity
                        .source()
                        .is_some_and(|source| source.ends_with(file))
                })
                .map(|(_, step)| *step)
        };
        assert_eq!(step_of("b_orders.json"), Some(2));
        assert_eq!(step_of("a_orders.json"), Some(1));
    }

    #[tokio::test]
    async fn test_hit_counts_stay_with_their_mock_across_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        write_mock(
            dir.path(),
            "b_orders.json",
            r#"{"method":"POST","path":"/orders","status":200,"response":{"who":"b"}}"#,
        );

        let store = load_mocks(dir.path().to_str().unwrap());
        let state = AppState::new(store.clone());
        post_orders(&state).await;
        post_orders(&state).await;

        write_mock(
            dir.path(),
            "a_orders.json",
            r#"{"method":"POST","path":"/orders","status":200,"response":{"who":"a"}}"#,
        );
        reload_cycle(&state, &store, dir.path()).await;

        let hits = state.mock_hits.read().await;
        let hits_for = |file: &str| {
            hits.iter()
                .find(|(identity, _)| identity.source().is_some_and(|s| s.ends_with(file)))
                .map(|(_, count)| *count)
        };
        assert_eq!(hits_for("b_orders.json"), Some(2));
        assert_eq!(
            hits_for("a_orders.json"),
            None,
            "a mock that has served nothing must not inherit another's count"
        );
    }

    #[tokio::test]
    async fn test_reload_drops_counters_for_a_deleted_mock() {
        let dir = tempfile::tempdir().unwrap();
        write_mock(dir.path(), "orders.json", &sequenced_orders_mock("b"));

        let store = load_mocks(dir.path().to_str().unwrap());
        let state = AppState::new(store.clone());
        post_orders(&state).await;
        assert_eq!(state.sequence_counters.read().await.len(), 1);

        std::fs::remove_file(dir.path().join("orders.json")).unwrap();
        reload_cycle(&state, &store, dir.path()).await;

        assert!(
            state.sequence_counters.read().await.is_empty(),
            "an orphaned counter must be dropped, not left for the next mock to adopt"
        );
        assert!(state.mock_hits.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_a_file_that_stops_parsing_keeps_its_counter() {
        // `apply_reload` carries a broken file's last-known-good route
        // forward, so the mock is still loaded and still serving — pruning
        // must not treat "failed to parse this cycle" as "deleted".
        let dir = tempfile::tempdir().unwrap();
        write_mock(dir.path(), "orders.json", &sequenced_orders_mock("b"));

        let store = load_mocks(dir.path().to_str().unwrap());
        let state = AppState::new(store.clone());
        assert_eq!(post_orders(&state).await["step"], 1);

        write_mock(dir.path(), "orders.json", "{ mid-save");
        reload_cycle(&state, &store, dir.path()).await;

        assert_eq!(
            post_orders(&state).await["step"],
            2,
            "the sequence should resume, not restart, while the file is being fixed"
        );
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
