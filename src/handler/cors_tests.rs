use super::*;
use crate::types::{BodyMatcher, JsonBodyMatcher, MockConfig};
use axum::http::Method;

fn mock(method: &str, path: &str) -> MockConfig {
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

fn state_with(mocks: Vec<MockConfig>) -> AppState {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for m in mocks {
        let key = crate::types::create_mock_key(&m.method, &m.path);
        map.entry(key).or_default().push(m);
    }
    AppState::new(Arc::new(tokio::sync::RwLock::new(map)))
}

/// A configuration built from the given `MIMIC_CORS_*` values, without
/// touching the process environment the rest of the suite shares.
fn config(pairs: &[(&str, &str)]) -> CorsConfig {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    CorsConfig::from_env(|key| {
        owned
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
    })
    .expect("these settings should enable CORS")
}

/// The default configuration: `MIMIC_CORS=true` and nothing else.
fn enabled() -> CorsConfig {
    config(&[("MIMIC_CORS", "true")])
}

async fn send(state: &AppState, method: Method, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut header_map = HeaderMap::new();
    for (name, value) in headers {
        header_map.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    handle_request(
        method,
        path.parse().unwrap(),
        header_map,
        State(state.clone()),
        Body::empty(),
    )
    .await
}

fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
}

const ORIGIN: (&str, &str) = ("origin", "http://localhost:3000");
const PREFLIGHT_POST: (&str, &str) = ("access-control-request-method", "POST");

// ── Off by default ──────────────────────────────────────────────────

#[tokio::test]
async fn cors_off_leaves_responses_exactly_as_they_were() {
    let state = state_with(vec![mock("GET", "/users")]);

    let response = send(&state, Method::GET, "/users", &[ORIGIN]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "access-control-allow-origin"), None);
    assert_eq!(header(&response, "vary"), None);
}

#[tokio::test]
async fn cors_off_still_404s_a_preflight() {
    let state = state_with(vec![mock("POST", "/users")]);

    let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Simple responses ────────────────────────────────────────────────

#[tokio::test]
async fn a_simple_response_carries_the_cors_headers() {
    let state = state_with(vec![mock("GET", "/users")]).with_cors(enabled());

    let response = send(&state, Method::GET, "/users", &[ORIGIN]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        Some("*".to_string())
    );
    assert_eq!(
        header(&response, "content-type"),
        Some("application/json".to_string()),
        "the CORS headers must not displace the content type"
    );
}

/// Mocking a CORS *failure* has to stay possible, so the mock's own header
/// is never replaced by the global config.
#[tokio::test]
async fn a_mocks_own_header_wins() {
    let mut deliberate = mock("GET", "/users");
    deliberate.response_headers = Some(HashMap::from([(
        "Access-Control-Allow-Origin".to_string(),
        "http://somewhere-else".to_string(),
    )]));
    let state = state_with(vec![deliberate]).with_cors(enabled());

    let response = send(&state, Method::GET, "/users", &[ORIGIN]).await;

    assert_eq!(
        header(&response, "access-control-allow-origin"),
        Some("http://somewhere-else".to_string())
    );
}

#[tokio::test]
async fn an_origin_outside_the_allowlist_gets_no_allow_origin_header() {
    let state = state_with(vec![mock("GET", "/users")]).with_cors(config(&[
        ("MIMIC_CORS", "true"),
        ("MIMIC_CORS_ORIGINS", "http://localhost:3000"),
    ]));

    let allowed = send(&state, Method::GET, "/users", &[ORIGIN]).await;
    assert_eq!(
        header(&allowed, "access-control-allow-origin"),
        Some("http://localhost:3000".to_string())
    );

    let refused = send(
        &state,
        Method::GET,
        "/users",
        &[("origin", "http://evil.example")],
    )
    .await;
    assert_eq!(refused.status(), StatusCode::OK);
    assert_eq!(header(&refused, "access-control-allow-origin"), None);
    assert_eq!(header(&refused, "vary"), Some("Origin".to_string()));
}

// ── Preflights ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_preflight_for_a_mocked_path_is_answered_without_a_mock_file() {
    let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

    let response = send(
        &state,
        Method::OPTIONS,
        "/users",
        &[
            ORIGIN,
            PREFLIGHT_POST,
            ("access-control-request-headers", "content-type"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        Some("*".to_string())
    );
    assert_eq!(
        header(&response, "access-control-allow-methods"),
        Some("GET, POST, PUT, PATCH, DELETE, OPTIONS".to_string())
    );
    assert_eq!(
        header(&response, "access-control-allow-headers"),
        Some("content-type".to_string())
    );
    assert_eq!(
        header(&response, "access-control-max-age"),
        Some("600".to_string())
    );
}

#[tokio::test]
async fn a_preflight_for_an_unmocked_path_still_404s() {
    let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

    let response = send(
        &state,
        Method::OPTIONS,
        "/nowhere",
        &[ORIGIN, PREFLIGHT_POST],
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The preflight asks about POST; only GET is mocked. Answering 204 here
/// would promise an endpoint that 404s a moment later.
#[tokio::test]
async fn a_preflight_for_an_unmocked_method_still_404s() {
    let state = state_with(vec![mock("GET", "/users")]).with_cors(enabled());

    let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A preflight carries no body, so running the endpoint's own matchers
/// against it would reject every preflight for a mock worth calling.
#[tokio::test]
async fn a_preflight_is_answered_for_an_endpoint_behind_a_body_matcher() {
    let mut guarded = mock("POST", "/login");
    guarded.consume_body = true;
    guarded.body = Some(BodyMatcher::Json(JsonBodyMatcher {
        exact: None,
        partial: Some(json!({"user": "admin"})),
        strict: false,
    }));
    let state = state_with(vec![guarded]).with_cors(enabled());

    let response = send(&state, Method::OPTIONS, "/login", &[ORIGIN, PREFLIGHT_POST]).await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_preflight_is_answered_for_a_pattern_route() {
    let state = state_with(vec![mock("DELETE", "/users/:id")]).with_cors(enabled());

    let response = send(
        &state,
        Method::OPTIONS,
        "/users/42",
        &[ORIGIN, ("access-control-request-method", "DELETE")],
    )
    .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// Gap-filling, not hijacking: a hand-written `OPTIONS` mock matches first
/// and never reaches the preflight path.
#[tokio::test]
async fn an_explicit_options_mock_wins() {
    let mut explicit = mock("OPTIONS", "/users");
    explicit.status = 418;
    let state = state_with(vec![mock("POST", "/users"), explicit]).with_cors(enabled());

    let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
}

/// An `OPTIONS` without `Access-Control-Request-Method` isn't a preflight —
/// it's curl, or a probe — and keeps the behavior it has always had.
#[tokio::test]
async fn a_bare_options_is_not_treated_as_a_preflight() {
    let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

    let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN]).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_preflight_from_a_disallowed_origin_gets_no_allow_origin_header() {
    let state = state_with(vec![mock("POST", "/users")]).with_cors(config(&[
        ("MIMIC_CORS", "true"),
        ("MIMIC_CORS_ORIGINS", "http://localhost:3000"),
    ]));

    let response = send(
        &state,
        Method::OPTIONS,
        "/users",
        &[("origin", "http://evil.example"), PREFLIGHT_POST],
    )
    .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(header(&response, "access-control-allow-origin"), None);
}

#[tokio::test]
async fn a_preflight_honours_a_scenario_filter() {
    let mut tagged = mock("POST", "/checkout");
    tagged.tags = vec!["error-scenario".to_string()];
    let map = HashMap::from([("POST:/checkout".to_string(), vec![tagged])]);
    let state = AppState::with_active_tags(
        Arc::new(tokio::sync::RwLock::new(map)),
        Some(HashSet::from(["happy-path".to_string()])),
    )
    .with_cors(enabled());

    let response = send(
        &state,
        Method::OPTIONS,
        "/checkout",
        &[ORIGIN, PREFLIGHT_POST],
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a mock the active scenario hides has no endpoint to preflight"
    );
}

// ── Observability ───────────────────────────────────────────────────

#[tokio::test]
async fn a_preflight_is_recorded_with_an_explanation() {
    let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

    send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

    let log = state.request_log.read().await;
    assert_eq!(log.len(), 1, "a preflight must not vanish from the log");
    let record = &log[0];
    assert_eq!(record.method, "OPTIONS");
    assert_eq!(record.path, "/users");
    assert_eq!(record.response_status, 204);
    assert_eq!(record.matched_mock, None);
    assert_eq!(
        record.match_explanation.as_deref(),
        Some(crate::cors::PREFLIGHT_EXPLANATION)
    );
    assert_eq!(
        record.response_headers.get("access-control-allow-origin"),
        Some(&"*".to_string())
    );
}

// ── Mimic's own hand-built responses (#103) ────────────────────────────

/// The `mock not found` 404 is the response a browser most needs to read —
/// it's what turns a forgotten mock into a readable message instead of an
/// opaque "blocked by CORS policy".
#[tokio::test]
async fn a_404_for_an_unmocked_path_carries_cors_headers() {
    let state = state_with(vec![mock("GET", "/users")]).with_cors(enabled());

    let response = send(&state, Method::GET, "/nope", &[ORIGIN]).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        Some("*".to_string())
    );
}

#[tokio::test]
async fn a_404_from_a_disallowed_origin_still_gets_no_allow_origin_header() {
    let state = state_with(vec![mock("GET", "/users")]).with_cors(config(&[
        ("MIMIC_CORS", "true"),
        ("MIMIC_CORS_ORIGINS", "http://localhost:3000"),
    ]));

    let response = send(
        &state,
        Method::GET,
        "/nope",
        &[("origin", "http://evil.example")],
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, "access-control-allow-origin"), None);
}

#[tokio::test]
async fn cors_off_leaves_the_404_exactly_as_it_was() {
    let state = state_with(vec![mock("GET", "/users")]);

    let response = send(&state, Method::GET, "/nope", &[ORIGIN]).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, "access-control-allow-origin"), None);
}

#[test]
fn the_401_from_a_missing_admin_token_carries_cors_headers() {
    let config = enabled();
    let response = unauthorized(Some(&config), Some("http://localhost:3000"));

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}

#[test]
fn the_413_from_an_oversized_body_carries_cors_headers() {
    let config = enabled();
    let response = payload_too_large(
        "POST",
        "/upload",
        1024,
        Some(&config),
        Some("http://localhost:3000"),
    );

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}
