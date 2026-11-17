//! Verification API tests (#149) — `count`, `reset`, `reload`, HAR export.

use super::*;

async fn record(state: &AppState, path: &str, status: u16, matched: Option<&str>) {
    let duration_ms = record_in(state, path, status, matched, None).await;
    let _ = duration_ms;
}

async fn record_in(
    state: &AppState,
    path: &str,
    status: u16,
    matched: Option<&str>,
    duration_ms: Option<u64>,
) -> Option<u64> {
    // Push a record straight into the log the way `handle_request` would.
    let record = types::RequestRecord {
        id: 0,
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        method: if matched.is_some_and(|m| m.starts_with("GET")) {
            "GET".to_string()
        } else {
            "POST".to_string()
        },
        path: path.to_string(),
        query_params: HashMap::new(),
        headers: HashMap::new(),
        body: None,
        matched_mock: matched.map(|m| m.to_string()),
        response_status: status,
        response_body: None,
        response_headers: HashMap::new(),
        match_score: None,
        path_params: HashMap::new(),
        match_explanation: None,
        duration_ms,
    };
    let mut log = state.request_log.write().await;
    let mut record = record;
    record.id = state
        .request_counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    let id = record.id;
    crate::handler::push_bounded(&mut log, record, 1000);
    Some(id)
}

#[tokio::test]
async fn test_count_returns_total_and_per_filters() {
    let state = state_serving(vec![]);
    record(&state, "/payments", 201, Some("POST:/payments")).await;
    record(&state, "/payments", 201, Some("POST:/payments")).await;
    record(&state, "/other", 404, None).await;

    let app = create_router(state);
    let (status, body) = call(app, "GET", "/admin/requests/count").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 3);
}

#[tokio::test]
async fn test_count_filter_by_path_and_method() {
    let state = state_serving(vec![]);
    record(&state, "/payments", 201, Some("POST:/payments")).await;
    record(&state, "/payments", 404, None).await;
    record(&state, "/users", 200, Some("GET:/users")).await;

    let (_, body) = call(
        create_router(state.clone()),
        "GET",
        "/admin/requests/count?path=/payments",
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 2);

    let (_, body) = call(
        create_router(state),
        "GET",
        "/admin/requests/count?method=GET",
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 1);
}

#[tokio::test]
async fn test_count_expect_asserts_with_417() {
    let state = state_serving(vec![]);
    record(&state, "/payments", 201, Some("POST:/payments")).await;

    let (status, _) = call(
        create_router(state.clone()),
        "GET",
        "/admin/requests/count?path=/payments&expect=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        create_router(state),
        "GET",
        "/admin/requests/count?path=/payments&expect=2",
    )
    .await;
    assert_eq!(status, StatusCode::EXPECTATION_FAILED);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["matched"], false);
}

#[tokio::test]
async fn test_reset_clears_log_hits_and_counters() {
    let state = state_serving(vec![]);
    record(&state, "/a", 200, Some("POST:/a")).await;
    {
        let mut hits = state.mock_hits.write().await;
        hits.insert(
            types::MockIdentity::of(
                &MockConfig {
                    method: "POST".into(),
                    path: "/a".into(),
                    status: 200,
                    ..Default::default()
                },
                "POST:/a",
                0,
            ),
            7,
        );
    }
    {
        let mut counters = state.sequence_counters.write().await;
        counters.insert(
            types::MockIdentity::of(
                &MockConfig {
                    method: "POST".into(),
                    path: "/a".into(),
                    status: 200,
                    ..Default::default()
                },
                "POST:/a",
                0,
            ),
            3,
        );
    }

    let app = create_router(state.clone());
    let (status, body) = call(app, "POST", "/admin/reset").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cleared_requests"], 1);
    assert_eq!(json["cleared_hits"], 1);
    assert_eq!(json["cleared_sequences"], 1);

    // And the count endpoint agrees the log is empty.
    let app = create_router(state);
    let (_, body) = call(app, "GET", "/admin/requests/count").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 0);
}

#[tokio::test]
async fn test_reload_without_background_loop_installs_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    write_mock(
        dir.path(),
        "a.json",
        r#"{"method":"GET","path":"/a","status":200,"response":{"ok":true}}"#,
    );

    let state = state_serving(vec![]).with_mocks_dir(dir.path().to_str().unwrap().to_string());
    let app = create_router(state.clone());

    let (status, body) = call(app, "POST", "/admin/reload").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reloaded"], true);
    assert_eq!(json["endpoints"], 1);

    // The store now serves the mock.
    let store = state.mocks.read().await;
    assert!(store.contains_key("GET:/a"));
}

#[tokio::test]
async fn test_har_export_contains_entries_and_content_type() {
    let state = state_serving(vec![]);
    record(&state, "/payments", 201, Some("POST:/payments")).await;

    let app = create_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin/requests.har")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("application/har+json")
    );

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["log"]["version"], "1.2");
    let entries = json["log"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["request"]["method"], "POST");
    assert_eq!(entries[0]["response"]["status"], 201);
}
