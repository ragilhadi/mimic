use super::*;
use axum::http::Method;
use std::fs;
use std::path::Path as FsPath;
use tempfile::TempDir;

/// A fixture that is unmistakably not text: every byte value, including a
/// NUL, a lone `0x80` continuation byte, and a `{{path.id}}` that must
/// never be treated as a template.
fn binary_fixture() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(b"{{path.id}}");
    bytes.extend((0u8..=255).rev());
    bytes
}

/// FNV-1a, so "byte-identical" is asserted on a digest of the whole body
/// rather than on its length.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(0x100000001b3)
    })
}

/// Write `mock` and its fixtures into a fresh mocks directory.
fn mocks_dir(files: &[(&str, &[u8])]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (name, contents) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }
    dir
}

/// Load a mocks directory exactly as the server does and serve from it.
fn state_from(dir: &FsPath) -> AppState {
    let result = crate::loader::load_mocks_map(dir.to_str().unwrap());
    assert_eq!(result.errors.len(), 0, "fixture set should load cleanly");
    AppState::new(Arc::new(tokio::sync::RwLock::new(result.mocks)))
}

async fn get(state: &AppState, path: &str) -> (StatusCode, HeaderMap, Bytes) {
    let response = handle_request(
        Method::GET,
        path.parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, parts.headers, bytes)
}

fn content_type(headers: &HeaderMap) -> &str {
    headers.get("content-type").unwrap().to_str().unwrap()
}

#[tokio::test]
async fn serves_a_text_fixture_with_a_content_type_inferred_from_its_extension() {
    let csv = "id,name\n1,Alice\n2,Bob\n";
    let dir = mocks_dir(&[
        (
            "export.json",
            br#"{"method":"GET","path":"/reports/export","status":200,
                 "response_file":"fixtures/report.csv"}"#,
        ),
        ("fixtures/report.csv", csv.as_bytes()),
    ]);

    let (status, headers, body) = get(&state_from(dir.path()), "/reports/export").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/csv; charset=utf-8");
    assert_eq!(body, Bytes::from(csv));
}

#[tokio::test]
async fn a_binary_fixture_round_trips_byte_identically() {
    let fixture = binary_fixture();
    let dir = mocks_dir(&[
        (
            "logo.json",
            br#"{"method":"GET","path":"/assets/logo.png","status":200,
                 "response_file":"fixtures/logo.png"}"#,
        ),
        ("fixtures/logo.png", &fixture),
    ]);

    let (status, headers, body) = get(&state_from(dir.path()), "/assets/logo.png").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "image/png");
    assert_eq!(
        checksum(&body),
        checksum(&fixture),
        "the served bytes must digest to the same value as the file"
    );
    assert_eq!(body.len(), fixture.len());
}

#[tokio::test]
async fn a_json_fixture_is_served_as_a_json_body_not_a_quoted_string() {
    let dir = mocks_dir(&[
        (
            "users.json",
            br#"{"method":"GET","path":"/users","status":200,
                 "response_file":"fixtures/users.json"}"#,
        ),
        ("fixtures/users.json", br#"{"users":[{"id":1}]}"#),
    ]);

    let (_, headers, body) = get(&state_from(dir.path()), "/users").await;

    assert_eq!(content_type(&headers), "application/json");
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["users"][0]["id"], 1);
}

#[tokio::test]
async fn response_headers_win_over_the_inferred_content_type() {
    let dir = mocks_dir(&[
        (
            "export.json",
            br#"{"method":"GET","path":"/export","status":200,
                 "response_file":"fixtures/report.bin",
                 "response_headers":{"Content-Type":"text/csv"}}"#,
        ),
        ("fixtures/report.bin", b"id,name\n1,Alice\n"),
    ]);

    let (_, headers, body) = get(&state_from(dir.path()), "/export").await;

    assert_eq!(content_type(&headers), "text/csv");
    assert_eq!(body, Bytes::from("id,name\n1,Alice\n"));
}

#[tokio::test]
async fn an_unknown_extension_falls_back_to_octet_stream() {
    let dir = mocks_dir(&[
        (
            "blob.json",
            br#"{"method":"GET","path":"/blob","status":200,
                 "response_file":"fixtures/thing.wat"}"#,
        ),
        ("fixtures/thing.wat", b"\x00\x01\x02"),
    ]);

    let (_, headers, _) = get(&state_from(dir.path()), "/blob").await;
    assert_eq!(content_type(&headers), "application/octet-stream");
}

#[tokio::test]
async fn templating_is_off_by_default_for_a_file_body() {
    let dir = mocks_dir(&[
        (
            "user.json",
            br#"{"method":"GET","path":"/users/:id/card","status":200,
                 "response_file":"fixtures/card.html"}"#,
        ),
        ("fixtures/card.html", b"<p>user {{path.id}}</p>"),
    ]);

    let (_, _, body) = get(&state_from(dir.path()), "/users/42/card").await;
    assert_eq!(
        body,
        Bytes::from("<p>user {{path.id}}</p>"),
        "a fixture must not be interpolated unless the mock asks for it"
    );
}

#[tokio::test]
async fn template_true_interpolates_a_text_fixture() {
    let dir = mocks_dir(&[
        (
            "user.json",
            br#"{"method":"GET","path":"/users/:id/card","status":200,
                 "template":true,
                 "response_file":"fixtures/card.html"}"#,
        ),
        (
            "fixtures/card.html",
            b"<p>user {{path.id}} sorted by {{query.sort}}</p>",
        ),
    ]);

    let (_, _, body) = get(&state_from(dir.path()), "/users/42/card?sort=name").await;
    assert_eq!(body, Bytes::from("<p>user 42 sorted by name</p>"));
}

#[tokio::test]
async fn templating_never_runs_on_a_binary_body() {
    // The fixture contains a literal `{{path.id}}`; a PNG is bytes, and
    // opting in must not change that.
    let fixture = binary_fixture();
    let dir = mocks_dir(&[
        (
            "logo.json",
            br#"{"method":"GET","path":"/assets/:id/logo.png","status":200,
                 "template":true,
                 "response_file":"fixtures/logo.png"}"#,
        ),
        ("fixtures/logo.png", &fixture),
    ]);

    let (_, headers, body) = get(&state_from(dir.path()), "/assets/42/logo.png").await;

    assert_eq!(content_type(&headers), "image/png");
    assert_eq!(checksum(&body), checksum(&fixture));
}

#[tokio::test]
async fn a_templated_file_body_can_read_the_request_body() {
    // `{{body.…}}` lives in the fixture, not in the mock JSON, so the
    // decision to read the request body has to look inside the file.
    let dir = mocks_dir(&[
        (
            "echo.json",
            br#"{"method":"POST","path":"/echo","status":200,
                 "template":true,
                 "response_file":"fixtures/echo.txt"}"#,
        ),
        ("fixtures/echo.txt", b"you said: {{body.message}}"),
    ]);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    let response = handle_request(
        Method::POST,
        "/echo".parse().unwrap(),
        headers,
        State(state_from(dir.path())),
        Body::from(r#"{"message":"hi"}"#),
    )
    .await;
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(body, Bytes::from("you said: hi"));
}

#[tokio::test]
async fn a_sequence_step_can_serve_a_file() {
    let dir = mocks_dir(&[
        (
            "flaky.json",
            br#"{"method":"GET","path":"/flaky","status":200,
                 "response":{"ok":true},
                 "sequence":[
                   {"status":503,"response":{"error":"unavailable"}},
                   {"status":200,"response_file":"fixtures/ok.csv","repeat":true}
                 ]}"#,
        ),
        ("fixtures/ok.csv", b"id\n1\n"),
    ]);
    let state = state_from(dir.path());

    let (status, _, _) = get(&state, "/flaky").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, headers, body) = get(&state, "/flaky").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/csv; charset=utf-8");
    assert_eq!(body, Bytes::from("id\n1\n"));
}

#[tokio::test]
async fn the_request_log_records_a_descriptor_for_a_binary_body() {
    let fixture = binary_fixture();
    let dir = mocks_dir(&[
        (
            "logo.json",
            br#"{"method":"GET","path":"/logo.png","status":200,
                 "response_file":"fixtures/logo.png"}"#,
        ),
        ("fixtures/logo.png", &fixture),
    ]);
    let state = state_from(dir.path());

    let (_, _, body) = get(&state, "/logo.png").await;

    let log = state.request_log.read().await;
    let recorded = log[0].response_body.as_deref().unwrap();
    assert!(
        recorded.contains(&format!("{} bytes", body.len()))
            && recorded.contains("image/png")
            && recorded.contains("fixtures/logo.png"),
        "unhelpful descriptor: {}",
        recorded
    );
    assert!(
        recorded.len() < 200,
        "the log must not carry the bytes: {} chars",
        recorded.len()
    );
}

#[tokio::test]
async fn the_request_log_keeps_a_text_body_verbatim() {
    let dir = mocks_dir(&[
        (
            "export.json",
            br#"{"method":"GET","path":"/export","status":200,
                 "response_file":"fixtures/report.csv"}"#,
        ),
        ("fixtures/report.csv", b"id,name\n1,Alice\n"),
    ]);
    let state = state_from(dir.path());

    let _ = get(&state, "/export").await;

    let log = state.request_log.read().await;
    assert_eq!(
        log[0].response_body.as_deref(),
        Some("id,name\n1,Alice\n"),
        "a CSV body is exactly what the response drawer is for"
    );
}

#[tokio::test]
async fn a_reload_picks_up_a_changed_fixture() {
    // The mock file is untouched; only the fixture changes. Bytes are read
    // at load time, so this is the case that proves they're re-read.
    let dir = mocks_dir(&[
        (
            "export.json",
            br#"{"method":"GET","path":"/export","status":200,
                 "response_file":"fixtures/report.csv"}"#,
        ),
        ("fixtures/report.csv", b"id\n1\n"),
    ]);
    let state = state_from(dir.path());

    let (_, _, body) = get(&state, "/export").await;
    assert_eq!(body, Bytes::from("id\n1\n"));

    fs::write(dir.path().join("fixtures/report.csv"), b"id\n1\n2\n").unwrap();
    let reloaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
    assert_eq!(reloaded.errors.len(), 0);
    *state.mocks.write().await = reloaded.mocks;

    let (_, _, body) = get(&state, "/export").await;
    assert_eq!(body, Bytes::from("id\n1\n2\n"));
}

// ------------------------------------------------------------------
// Content-type inference, in isolation
// ------------------------------------------------------------------

#[test]
fn content_type_inference_covers_the_documented_extensions() {
    for (file, expected) in [
        ("a/b/report.json", "application/json"),
        ("feed.xml", "application/xml"),
        ("report.csv", "text/csv; charset=utf-8"),
        ("page.html", "text/html; charset=utf-8"),
        ("notes.txt", "text/plain; charset=utf-8"),
        ("logo.PNG", "image/png"),
        ("photo.jpg", "image/jpeg"),
        ("photo.jpeg", "image/jpeg"),
        ("invoice.pdf", "application/pdf"),
        ("export.zip", "application/zip"),
        ("mystery.bin", "application/octet-stream"),
        ("no_extension", "application/octet-stream"),
    ] {
        assert_eq!(content_type_for_file(file), expected, "for {}", file);
    }
}

#[test]
fn only_textual_content_types_are_templated_and_logged() {
    for textual in [
        "text/plain; charset=utf-8",
        "text/csv",
        "application/json",
        "application/xml",
        "application/soap+xml",
        "text/html",
    ] {
        assert!(is_textual_content_type(textual), "{} is text", textual);
    }
    for binary in [
        "image/png",
        "application/pdf",
        "application/zip",
        "application/octet-stream",
    ] {
        assert!(!is_textual_content_type(binary), "{} is not text", binary);
    }
}
