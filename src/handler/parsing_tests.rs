//! Request parsing and fixture escaping (#138): what the matcher and the
//! templater see of a path, a header, a content type, and a templated body.

use super::*;
use std::fs;
use tempfile::TempDir;

fn mocks_dir(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (name, contents) in files {
        fs::write(dir.path().join(name), contents).unwrap();
    }
    dir
}

fn state_from(dir: &std::path::Path) -> AppState {
    let result = crate::loader::load_mocks_map(dir.to_str().unwrap());
    assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
    AppState::new(Arc::new(tokio::sync::RwLock::new(result.mocks)))
}

async fn call(
    state: &AppState,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, String) {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    let response = handle_request(
        method,
        path.parse().unwrap(),
        map,
        State(state.clone()),
        Body::from(body.to_string()),
    )
    .await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, String::from_utf8_lossy(&bytes).to_string())
}

// ── Percent-encoded paths ───────────────────────────────────────────

#[tokio::test]
async fn a_percent_encoded_path_parameter_is_decoded() {
    let dir = mocks_dir(&[(
        "u.json",
        r#"{"method":"GET","path":"/users/:name","status":200,
            "response":{"name":"{{path.name}}"}}"#,
    )]);
    let state = state_from(dir.path());

    let (_, body) = call(&state, Method::GET, "/users/john%20doe", &[], "").await;
    assert_eq!(body, r#"{"name":"john doe"}"#);

    let (_, body) = call(&state, Method::GET, "/users/caf%C3%A9", &[], "").await;
    assert_eq!(body, r#"{"name":"café"}"#);
}

#[tokio::test]
async fn a_mock_declared_with_a_decoded_path_matches_the_encoded_request() {
    let dir = mocks_dir(&[
        (
            "a.json",
            r#"{"method":"GET","path":"/files/a b.txt","status":200,"response":"space"}"#,
        ),
        (
            "b.json",
            r#"{"method":"GET","path":"/café","status":200,"response":"accent"}"#,
        ),
    ]);
    let state = state_from(dir.path());

    let (status, body) = call(&state, Method::GET, "/files/a%20b.txt", &[], "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "\"space\""));
    let (status, body) = call(&state, Method::GET, "/caf%C3%A9", &[], "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "\"accent\""));
}

#[tokio::test]
async fn an_encoded_slash_stays_inside_one_segment() {
    let dir = mocks_dir(&[(
        "f.json",
        r#"{"method":"GET","path":"/files/:name","status":200,
            "response":{"name":"{{path.name}}"}}"#,
    )]);
    let state = state_from(dir.path());

    let (status, body) = call(&state, Method::GET, "/files/a%2Fb", &[], "").await;
    assert_eq!(status, StatusCode::OK, "%2F must not split the segment");
    assert_eq!(body, r#"{"name":"a%2Fb"}"#);

    let (status, _) = call(&state, Method::GET, "/files/a/b", &[], "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_request_log_records_the_decoded_path() {
    let dir = mocks_dir(&[(
        "u.json",
        r#"{"method":"GET","path":"/users/:name","status":200,"response":{}}"#,
    )]);
    let state = state_from(dir.path());
    call(&state, Method::GET, "/users/john%20doe", &[], "").await;

    assert_eq!(state.request_log.read().await[0].path, "/users/john doe");
}

#[tokio::test]
async fn the_proxy_forwards_the_path_exactly_as_the_client_sent_it() {
    use axum::routing::get;
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let app = axum::Router::new().fallback(get(move |uri: Uri| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(uri.path().to_string());
            "ok"
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut state = AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())));
    state.proxy_config = Some(Arc::new(crate::proxy::ProxyConfig::for_test(
        format!("http://{}", addr),
        false,
        2000,
    )));

    let (status, _) = call(&state, Method::GET, "/users/john%20doe/a%2Fb", &[], "").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen.lock().unwrap()[0], "/users/john%20doe/a%2Fb");
}

// ── Repeated headers ────────────────────────────────────────────────

#[tokio::test]
async fn a_repeated_request_header_keeps_every_value() {
    let dir = mocks_dir(&[(
        "h.json",
        r#"{"method":"GET","path":"/h","status":200,
            "response":{"tag":"{{header.x-tag}}"}}"#,
    )]);
    let state = state_from(dir.path());

    let (_, body) = call(
        &state,
        Method::GET,
        "/h",
        &[("x-tag", "a"), ("x-tag", "b")],
        "",
    )
    .await;

    assert_eq!(body, r#"{"tag":"a, b"}"#);
}

#[tokio::test]
async fn a_header_matcher_can_see_a_value_sent_on_an_earlier_line() {
    let dir = mocks_dir(&[(
        "h.json",
        r#"{"method":"GET","path":"/h","status":200,"response":"hit",
            "headers":{"required":{"x-tag":{"contains":"a"}}}}"#,
    )]);
    let state = state_from(dir.path());

    let (status, _) = call(
        &state,
        Method::GET,
        "/h",
        &[("x-tag", "a"), ("x-tag", "b")],
        "",
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the first line used to be overwritten"
    );
}

// ── Content types ───────────────────────────────────────────────────

#[tokio::test]
async fn json_bodies_are_templated_whatever_the_content_type_spelling() {
    let dir = mocks_dir(&[(
        "e.json",
        r#"{"method":"POST","path":"/echo","status":200,
            "response":{"n":"{{body.name}}"}}"#,
    )]);
    let state = state_from(dir.path());

    for content_type in [
        "application/json",
        "Application/JSON",
        "application/json; charset=UTF-8",
        "application/vnd.api+json",
        "application/problem+json",
        "application/ld+json; profile=x",
    ] {
        let (_, body) = call(
            &state,
            Method::POST,
            "/echo",
            &[("content-type", content_type)],
            r#"{"name":"bob"}"#,
        )
        .await;
        assert_eq!(body, r#"{"n":"bob"}"#, "content-type: {content_type}");
    }
}

#[tokio::test]
async fn form_bodies_are_templated_whatever_the_content_type_case() {
    let dir = mocks_dir(&[(
        "f.json",
        r#"{"method":"POST","path":"/f","status":200,
            "response":{"n":"{{body.name}}"}}"#,
    )]);
    let state = state_from(dir.path());

    let (_, body) = call(
        &state,
        Method::POST,
        "/f",
        &[(
            "content-type",
            "Application/X-WWW-Form-Urlencoded; charset=utf-8",
        )],
        "name=bob",
    )
    .await;

    assert_eq!(body, r#"{"n":"bob"}"#);
}

// ── Escaping in templated fixtures ──────────────────────────────────

const JSON_NAME: &str = r#"{"name":"a\"b\\c"}"#;

fn fixture_dir(fixture: &str, mock_extra: &str, headers: &str) -> TempDir {
    mocks_dir(&[
        (
            "t.json",
            &format!(
                r#"{{"method":"POST","path":"/t","status":200,"template":true,
                    "response_file":"{fixture}"{mock_extra}{headers}}}"#,
            ),
        ),
        (
            fixture,
            match fixture {
                "body.json" => r#"{"n":"{{body.name}}"}"#,
                "body.xml" => "<n a='{{body.name}}'>{{body.name}}</n>",
                _ => "{{body.name}}",
            },
        ),
    ])
}

#[tokio::test]
async fn a_templated_json_fixture_stays_valid_json() {
    let dir = fixture_dir("body.json", "", "");
    let (_, body) = call(
        &state_from(dir.path()),
        Method::POST,
        "/t",
        &[("content-type", "application/json")],
        JSON_NAME,
    )
    .await;

    let parsed: serde_json::Value = serde_json::from_str(&body).expect(&body);
    assert_eq!(parsed["n"], r#"a"b\c"#);
}

#[tokio::test]
async fn a_templated_xml_fixture_escapes_markup() {
    let dir = fixture_dir("body.xml", "", "");
    let (_, body) = call(
        &state_from(dir.path()),
        Method::POST,
        "/t",
        &[("content-type", "application/json")],
        r#"{"name":"<b>&'\"x"}"#,
    )
    .await;

    assert_eq!(
        body,
        "<n a='&lt;b&gt;&amp;&#39;&quot;x'>&lt;b&gt;&amp;&#39;&quot;x</n>"
    );
}

#[tokio::test]
async fn a_templated_csv_fixture_is_not_escaped() {
    let dir = fixture_dir("body.csv", "", "");
    let (_, body) = call(
        &state_from(dir.path()),
        Method::POST,
        "/t",
        &[("content-type", "application/json")],
        JSON_NAME,
    )
    .await;

    assert_eq!(body, r#"a"b\c"#);
}

#[tokio::test]
async fn template_escape_false_restores_raw_interpolation() {
    let dir = fixture_dir("body.json", r#","template_escape":false"#, "");
    let (_, body) = call(
        &state_from(dir.path()),
        Method::POST,
        "/t",
        &[("content-type", "application/json")],
        r#"{"name":"1,\"x\":2"}"#,
    )
    .await;

    assert_eq!(
        body, r#"{"n":"1,"x":2"}"#,
        "pre-formed JSON is spliced in as is"
    );
}

#[tokio::test]
async fn escaping_follows_a_content_type_set_by_response_headers() {
    let dir = fixture_dir(
        "body.csv",
        "",
        r#","response_headers":{"Content-Type":"application/json"}"#,
    );
    let (_, body) = call(
        &state_from(dir.path()),
        Method::POST,
        "/t",
        &[("content-type", "application/json")],
        JSON_NAME,
    )
    .await;

    assert_eq!(body, r#"a\"b\\c"#);
}
