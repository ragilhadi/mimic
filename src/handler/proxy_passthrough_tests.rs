//! Proxy passthrough behaviour and the request log's handling of proxied and
//! binary bodies (#139).

use super::*;
use crate::proxy::ProxyConfig;
use axum::routing::get;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

async fn spawn(app: axum::Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{}", addr)
}

fn proxy_state(upstream: &str, record_dir: Option<&std::path::Path>, buffer: usize) -> AppState {
    let mut state = AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())));
    let mut cfg = ProxyConfig::for_test(upstream.to_string(), record_dir.is_some(), 2000);
    cfg.buffer_limit = buffer;
    state.proxy_config = Some(Arc::new(cfg));
    if let Some(dir) = record_dir {
        state.mocks_dir = dir.to_str().unwrap().to_string();
    }
    state
}

async fn request(state: &AppState, path: &str) -> Response {
    handle_request(
        Method::GET,
        path.parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await
}

#[tokio::test]
async fn an_upstream_redirect_reaches_the_client_unfollowed() {
    let upstream = spawn(axum::Router::new().route(
        "/old",
        get(|| async {
            (
                StatusCode::FOUND,
                [(axum::http::header::LOCATION, "/new")],
                "",
            )
        }),
    ))
    .await;
    let state = proxy_state(&upstream, None, 1024);

    let response = request(&state, "/old").await;

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["location"], "/new");
    assert_eq!(state.request_log.read().await[0].response_status, 302);
}

#[tokio::test]
async fn a_redirect_is_recorded_as_a_redirect() {
    let upstream = spawn(axum::Router::new().route(
        "/old",
        get(|| async {
            (
                StatusCode::MOVED_PERMANENTLY,
                [(axum::http::header::LOCATION, "/new")],
                "",
            )
        }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = proxy_state(&upstream, Some(dir.path()), 1024);

    request(&state, "/old").await;
    // Recording runs detached.
    let recorded = dir.path().join("_recorded");
    for _ in 0..50 {
        if recorded.exists() && std::fs::read_dir(&recorded).unwrap().next().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let file = std::fs::read_dir(&recorded)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let mock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    assert_eq!(mock["status"], 301);
    assert_eq!(mock["response_headers"]["location"], "/new");
}

#[tokio::test]
async fn a_response_past_the_buffer_limit_is_streamed_intact_and_not_recorded() {
    let big: String = "0123456789abcdef".repeat(4096); // 64 KiB
    let served = big.clone();
    let upstream = spawn(axum::Router::new().route(
        "/big",
        get(move || {
            let served = served.clone();
            async move { ([(axum::http::header::CONTENT_TYPE, "text/plain")], served) }
        }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = proxy_state(&upstream, Some(dir.path()), 1024);

    let response = request(&state, "/big").await;
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(body.len(), big.len());
    assert_eq!(body, Bytes::from(big));
    let logged = state.request_log.read().await[0]
        .response_body
        .clone()
        .unwrap();
    assert!(
        logged.starts_with("<streamed response of text/plain"),
        "{logged}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !dir.path().join("_recorded").exists(),
        "a streamed response must not be recorded"
    );
}

#[tokio::test]
async fn a_response_within_the_limit_is_buffered_logged_and_recorded() {
    let upstream = spawn(axum::Router::new().route(
        "/small",
        get(|| async { ([(axum::http::header::CONTENT_TYPE, "text/plain")], "hello") }),
    ))
    .await;
    let state = proxy_state(&upstream, None, 1024);

    let response = request(&state, "/small").await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        Bytes::from("hello")
    );
    assert_eq!(
        state.request_log.read().await[0].response_body.as_deref(),
        Some("hello")
    );
}

#[tokio::test]
async fn an_event_stream_reaches_the_client_as_it_is_produced() {
    let (tx, rx) = tokio::sync::mpsc::channel::<&'static str>(4);
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let upstream = spawn(axum::Router::new().route(
        "/events",
        get(move || {
            let rx = rx.clone();
            async move {
                let rx = rx.lock().await.take().unwrap();
                let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                    rx.recv()
                        .await
                        .map(|chunk| (Ok::<_, std::io::Error>(Bytes::from(chunk)), rx))
                });
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream),
                )
            }
        }),
    ))
    .await;
    let state = proxy_state(&upstream, None, 10 * 1024 * 1024);

    tx.send("data: one\n\n").await.unwrap();
    let started = Instant::now();
    let response = request(&state, "/events").await;
    let mut body = response.into_body();

    // The first event has to arrive while the upstream is still open;
    // buffering to the limit (or to the end) would hang here.
    let first = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("the first event should not wait for the stream to end")
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), Bytes::from("data: one\n\n"));
    assert!(started.elapsed() < Duration::from_secs(1));

    tx.send("data: two\n\n").await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second.into_data().unwrap(), Bytes::from("data: two\n\n"));
}

#[tokio::test]
async fn a_slow_body_that_keeps_making_progress_outlives_the_timeout() {
    // Five chunks 150 ms apart: 750 ms in all, against a 300 ms timeout
    // that now applies to each wait rather than to the whole exchange.
    let upstream = spawn(axum::Router::new().route(
        "/slow",
        get(|| async {
            let stream = futures_util::stream::unfold(0u8, |n| async move {
                if n == 5 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                Some((Ok::<_, std::io::Error>(Bytes::from(format!("{n};"))), n + 1))
            });
            Body::from_stream(stream)
        }),
    ))
    .await;
    let mut state = proxy_state(&upstream, None, 10 * 1024 * 1024);
    state.proxy_config = Some(Arc::new(ProxyConfig::for_test(
        upstream.clone(),
        false,
        300,
    )));

    let response = request(&state, "/slow").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        Bytes::from("0;1;2;3;4;")
    );
}

#[tokio::test]
async fn a_binary_upstream_body_is_logged_as_a_descriptor() {
    let upstream = spawn(axum::Router::new().route(
        "/bin",
        get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                vec![0xffu8, 0xfe, 0x00, 0x01],
            )
        }),
    ))
    .await;
    let state = proxy_state(&upstream, None, 1024);

    let response = request(&state, "/bin").await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        Bytes::from_static(&[0xff, 0xfe, 0x00, 0x01])
    );
    assert_eq!(
        state.request_log.read().await[0].response_body.as_deref(),
        Some("<4 bytes of application/octet-stream>")
    );
}

#[tokio::test]
async fn a_binary_request_body_is_logged_as_a_descriptor_not_dropped() {
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())));
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "image/png".parse().unwrap());

    handle_request(
        Method::POST,
        "/upload".parse().unwrap(),
        headers,
        State(state.clone()),
        Body::from(vec![0x89u8, 0x50, 0x4e, 0x47, 0xff, 0xfe]),
    )
    .await;

    assert_eq!(
        state.request_log.read().await[0].body.as_deref(),
        Some("<6 bytes of image/png>")
    );
}

#[tokio::test]
async fn an_empty_request_body_is_still_logged_as_none() {
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())));
    request(&state, "/nothing").await;
    assert_eq!(state.request_log.read().await[0].body, None);
}

// ── Admin token ─────────────────────────────────────────────────────

#[test]
fn bearer_token_accepts_any_case_of_the_scheme() {
    assert_eq!(bearer_token("Bearer abc"), Some("abc"));
    assert_eq!(bearer_token("bearer abc"), Some("abc"));
    assert_eq!(bearer_token("BEARER   abc "), Some("abc"));
    assert_eq!(bearer_token("Bearer"), None);
    assert_eq!(bearer_token("Bearer   "), None);
    assert_eq!(bearer_token("Basic abc"), None);
    assert_eq!(bearer_token("abc"), None);
}

#[test]
fn constant_time_eq_agrees_with_equality() {
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(constant_time_eq(b"", b""));
    assert!(!constant_time_eq(b"secret", b"secreT"));
    assert!(!constant_time_eq(b"secret", b"secre"));
    assert!(!constant_time_eq(b"secre", b"secret"));
    assert!(!constant_time_eq(b"secret", b""));
}

#[tokio::test]
async fn the_admin_token_is_accepted_with_a_lowercase_scheme() {
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())))
        .with_admin_token(Some("tok".to_string()));
    let app = axum::Router::new()
        .route("/admin/mocks", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_admin_token,
        ));

    let status = |auth: Option<&'static str>| {
        let app = app.clone();
        async move {
            use tower::ServiceExt;
            let mut req = axum::http::Request::builder().uri("/admin/mocks");
            if let Some(value) = auth {
                req = req.header("authorization", value);
            }
            app.oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };

    assert_eq!(status(Some("Bearer tok")).await, StatusCode::OK);
    assert_eq!(status(Some("bearer tok")).await, StatusCode::OK);
    assert_eq!(status(Some("BEARER tok")).await, StatusCode::OK);
    assert_eq!(status(Some("Bearer tok2")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(Some("Bearer to")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(Some("Basic tok")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(None).await, StatusCode::UNAUTHORIZED);
}
