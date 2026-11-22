//! Runtime mock management tests (#153) — create/update/delete/toggle via
//! the admin API, persistence, readonly lock, hot-reload carry.

use super::*;

fn runtime_state(mocks: Vec<MockConfig>) -> AppState {
    let dir = tempfile::tempdir().unwrap();
    state_serving(mocks).with_mocks_dir(dir.path().to_str().unwrap().to_string())
}

async fn post_json(state: AppState, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let app = create_router(state);
    let body = body.to_string();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri.to_string())
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

async fn call_method(
    state: AppState,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let app = create_router(state);
    let body = body.unwrap_or_default().to_string();
    let mut builder = Request::builder().method(method).uri(uri.to_string());
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn test_post_mocks_registers_and_serves_on_next_request() {
    let state = runtime_state(vec![]);
    let (status, json) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/dyn","status":200,"response":{"live":true}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert!(json["id"].as_str().unwrap().starts_with("r-"));
    assert_eq!(json["key"], "GET:/dyn");

    // The store registered it — find_matching_mock sees it immediately.
    let store = state.mocks.read().await;
    assert!(store.contains_key("GET:/dyn"));
    assert_eq!(
        store["GET:/dyn"][0].source.as_deref(),
        Some("(runtime)@r-1")
    );
}

#[tokio::test]
async fn test_post_mocks_rejects_invalid_mock_with_loader_message() {
    let state = runtime_state(vec![]);
    let (status, json) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"","path":"/x","status":200,"response":{}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["detail"].as_str().unwrap().contains("Empty method"));

    let (status, json) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/x","status":200,"response":{},"response_file":"body.txt"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["detail"].as_str().unwrap().contains("both `response`"));
}

#[tokio::test]
async fn test_delete_mocks_removes_it_immediately() {
    let state = runtime_state(vec![]);
    let (_, created) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/tmp-x","status":200,"response":{}}"#,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, json) =
        call_method(state.clone(), "DELETE", &format!("/admin/mocks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["deleted"], true);

    let store = state.mocks.read().await;
    assert!(
        !store.contains_key("GET:/tmp-x"),
        "bucket removed when empty"
    );
}

#[tokio::test]
async fn test_delete_unknown_id_is_404() {
    let state = runtime_state(vec![]);
    let (status, json) = call_method(state, "DELETE", "/admin/mocks/r-999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "runtime mock not found");
}

#[tokio::test]
async fn test_unshaped_ids_are_rejected_untouched() {
    // A minted id is always `r-<digits>`; anything else must never flow into
    // a filesystem path (`..` segments, non-minted shapes), for DELETE, PUT
    // and the toggles alike. The response differs by where the request dies:
    // a single-segment id reaches the handler and gets the specific
    // "runtime mock not found"; a path with separators never matches the
    // `{id}` route at all and falls through to the mock pipeline's 404.
    // Both are 404s and neither touches the filesystem.
    for (method, uri) in [
        ("DELETE", "/admin/mocks/.."),
        ("DELETE", "/admin/mocks/x-1"),
        ("PUT", "/admin/mocks/x-1"),
        ("POST", "/admin/mocks/x-1/enable"),
    ] {
        let state = runtime_state(vec![]);
        let (status, json) = call_method(
            state,
            method,
            uri,
            Some(r#"{"method":"GET","path":"/x","status":200}"#),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {json}");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|e| e.ends_with("mock not found")),
            "{method} {uri}: {json}"
        );
    }
}

#[tokio::test]
async fn test_well_shaped_but_unknown_id_also_404s() {
    // Minted-shaped ids that simply don't exist behave like any other miss.
    let state = runtime_state(vec![]);
    let (status, _) = call_method(state, "DELETE", "/admin/mocks/r-42", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_put_mocks_updates_same_identity_and_can_move_paths() {
    let state = runtime_state(vec![]);
    let (_, created) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/edit","status":200,"response":{"v":1}}"#,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    let source_before: String = {
        let store = state.mocks.read().await;
        store["GET:/edit"][0].source.clone().unwrap()
    };

    let (status, json) = call_method(
        state.clone(),
        "PUT",
        &format!("/admin/mocks/{id}"),
        Some(r#"{"method":"GET","path":"/edit2","status":201,"response":{"v":2}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["moved"], true);
    assert_eq!(json["key"], "GET:/edit2");

    let store = state.mocks.read().await;
    assert!(!store.contains_key("GET:/edit"), "old bucket dropped");
    assert_eq!(store["GET:/edit2"][0].status, 201);
    assert_eq!(
        store["GET:/edit2"][0].source.as_deref(),
        Some(source_before.as_str())
    );
}

#[tokio::test]
async fn test_disable_then_enable_toggles_matching() {
    let state = runtime_state(vec![]);
    let (_, created) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/toggle","status":200,"response":{}}"#,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, json) = call_method(
        state.clone(),
        "POST",
        &format!("/admin/mocks/{id}/disable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["disabled"], true);

    {
        let store = state.mocks.read().await;
        let mock = &store["GET:/toggle"][0];
        assert_eq!(mock.disabled, Some(true));
        assert!(
            !mock.is_active(None),
            "disabled must be invisible to matching even with no scenario filter"
        );
    }

    let (status, _) = call_method(
        state.clone(),
        "POST",
        &format!("/admin/mocks/{id}/enable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    {
        let store = state.mocks.read().await;
        assert_eq!(store["GET:/toggle"][0].disabled, Some(false));
        assert!(store["GET:/toggle"][0].is_active(None));
    }
}

#[tokio::test]
async fn test_persist_writes_file_and_delete_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_serving(vec![]).with_mocks_dir(dir.path().to_str().unwrap().to_string());
    let (_, created) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/disk","status":200,"response":{"on":true},"persist":true}"#,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    let persisted = dir.path().join("_runtime").join(format!("{id}.json"));
    assert!(persisted.exists(), "persist=true must write the file");
    let on_disk: MockConfig = serde_json::from_slice(&std::fs::read(&persisted).unwrap()).unwrap();
    assert!(
        on_disk.source.is_none(),
        "the persisted file must not carry the runtime source; loader re-stamps identity"
    );

    let (status, _) =
        call_method(state.clone(), "DELETE", &format!("/admin/mocks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!persisted.exists(), "delete removes the persisted file too");
}

#[tokio::test]
async fn test_readonly_locks_the_write_routes() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_serving(vec![])
        .with_mocks_dir(dir.path().to_str().unwrap().to_string())
        .with_reserved(handler::ReservedRoutes {
            admin_readonly: true,
            ..Default::default()
        });

    for (method, uri) in [
        ("POST", "/admin/mocks"),
        ("PUT", "/admin/mocks/r-1"),
        ("DELETE", "/admin/mocks/r-1"),
        ("POST", "/admin/mocks/r-1/disable"),
    ] {
        let (status, json) = call_method(
            state.clone(),
            method,
            uri,
            Some(r#"{"method":"GET","path":"/x","status":200}"#),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {json}");
        assert_eq!(json["error"], "mock not found");
    }
}

#[tokio::test]
async fn test_runtime_mocks_survive_a_hot_reload_cycle() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("file.json"),
        r#"{"method":"GET","path":"/from-file","status":200,"response":{}}"#,
    )
    .unwrap();
    let state = state_serving(vec![]).with_mocks_dir(dir.path().to_str().unwrap().to_string());

    let (_, created) = post_json(
        state.clone(),
        "/admin/mocks",
        r#"{"method":"GET","path":"/runtime","status":418,"response":{}}"#,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // One reload cycle, exactly as `POST /admin/reload` runs it when no
    // background loop exists.
    let result = loader::load_mocks_map(dir.path().to_str().unwrap());
    let live = {
        let runtime = state.runtime_mocks.read().await;
        let runtime = runtime.clone();
        let mut store = state.mocks.write().await;
        *store = apply_reload(&store, result, &runtime);
        // Identity computed from the installed map itself — no second lock
        // acquisition while the write guard is held (that deadlocks).
        live_identities(&store)
    };
    let runtime_identity = {
        let store = state.mocks.read().await;
        types::MockIdentity::of(&store["GET:/runtime"][0], "GET:/runtime", 0)
    };
    assert!(
        live.contains(&runtime_identity),
        "the runtime mock's identity survives pruning — it is still live"
    );

    let store = state.mocks.read().await;
    assert!(
        store.contains_key("GET:/runtime"),
        "runtime layer re-merged"
    );
    assert_eq!(store["GET:/runtime"][0].status, 418);
    assert_eq!(
        store["GET:/runtime"][0].source.as_deref(),
        Some(format!("(runtime)@{id}").as_str())
    );
    assert!(store.contains_key("GET:/from-file"), "file layer untouched");
}

#[tokio::test]
async fn test_admin_route_matching_accepts_id_shaped_paths() {
    let routes = handler::ReservedRoutes::default();
    // Id segment: any single segment value.
    assert!(routes.reservation_for("PUT", "/admin/mocks/r-1").is_some());
    assert!(routes
        .reservation_for("POST", "/admin/mocks/r-1/enable")
        .is_some());
    // Not a *deeper* path, and the bare route still matches only bare.
    assert!(routes.reservation_for("GET", "/admin/mocks").is_some());
    assert_eq!(
        routes.reservation_for("GET", "/admin/mocks/r-1/extra"),
        None
    );
    assert_eq!(routes.reservation_for("GET", "/admin/mocks/"), None);
}

#[tokio::test]
async fn persisted_mock_updates_once_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let state = state_serving(vec![]).with_mocks_dir(root.to_string());
    let (status, created) = post_json(
        state.clone(),
        "/admin/mocks?persist=true",
        r#"{"method":"GET","path":"/old","status":200}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["persisted"], true);
    let id = created["id"].as_str().unwrap();
    let endpoint = format!("/admin/mocks/{id}");
    assert_eq!(
        call_method(
            state.clone(),
            "PUT",
            &endpoint,
            Some(r#"{"method":"GET","path":"/new","status":202}"#)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call_method(state.clone(), "POST", &format!("{endpoint}/disable"), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call_method(state.clone(), "POST", "/admin/reload", None)
            .await
            .0,
        StatusCode::OK
    );
    let (_, list) = call_method(state.clone(), "GET", "/admin/mocks", None).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["runtime_writes_enabled"], true);
    assert_eq!(list["mocks"][0]["id"], id);
    assert_eq!(list["mocks"][0]["disabled"], true);
    assert_eq!(
        call_method(state.clone(), "GET", "/new", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call_method(state, "POST", &format!("{endpoint}/enable"), None)
            .await
            .0,
        StatusCode::OK
    );
    let restarted =
        handler::AppState::new(loader::load_mocks(root)).with_mocks_dir(root.to_string());
    assert_eq!(
        call_method(restarted.clone(), "GET", "/new", None).await.0,
        StatusCode::ACCEPTED
    );
    let (_, next) = post_json(
        restarted,
        "/admin/mocks",
        r#"{"method":"GET","path":"/other","status":200}"#,
    )
    .await;
    assert_ne!(next["id"], id);
}

#[tokio::test]
async fn runtime_write_routes_require_the_admin_token() {
    let state = runtime_state(vec![]).with_admin_token(Some("expected".into()));
    let app = create_router(state);
    for (method, uri) in [
        ("POST", "/admin/mocks"),
        ("PUT", "/admin/mocks/r-1"),
        ("DELETE", "/admin/mocks/r-1"),
        ("POST", "/admin/mocks/r-1/disable"),
        ("POST", "/admin/mocks/r-1/enable"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
