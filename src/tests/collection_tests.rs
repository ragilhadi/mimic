//! Stateful CRUD collections tests (#153b) — the five routes, id handling,
//! reset-to-seed, reload carry, and load-time validation.

use super::*;

/// A state whose mock store declares one collection `/users` with two seed
/// rows, from a temp dir so reload tests can write files.
fn collection_state() -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("users.json"),
        r#"{"method":"GET","path":"/ignored-by-collection","collection":{
            "name": "users", "path": "/users", "id_field": "id",
            "seed": [{"id":1,"name":"Ada"},{"id":2,"name":"Grace"}]
        }}"#,
    )
    .unwrap();
    let result = loader::load_mocks(dir.path().to_str().unwrap());
    let state =
        handler::AppState::new(result).with_mocks_dir(dir.path().to_str().unwrap().to_string());
    (state, dir)
}

async fn http(
    state: &AppState,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value, Option<String>) {
    let app = create_router(state.clone());
    let mut req = Request::builder().method(method).uri(path);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let response = app
        .oneshot(
            req.body(Body::from(body.unwrap_or_default().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let location = response
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string());
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, location)
}

#[tokio::test]
async fn test_list_returns_seed_rows() {
    let (state, _dir) = collection_state();
    let (status, body, _) = http(&state, "GET", "/users", None).await;
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(body[0]["name"], "Ada");
}

#[tokio::test]
async fn test_get_one_by_numeric_id() {
    let (state, _dir) = collection_state();
    let (status, body, _) = http(&state, "GET", "/users/2", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["name"], "Grace");

    let (status, _, _) = http(&state, "GET", "/users/99", None).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn test_post_creates_with_assigned_id_and_location() {
    let (state, _dir) = collection_state();
    let (status, body, location) =
        http(&state, "POST", "/users", Some(r#"{"name":"Linus"}"#)).await;
    assert_eq!(status, 201);
    assert_eq!(body["id"], 3, "seed max is 2, next is 3");
    assert_eq!(body["name"], "Linus");
    assert_eq!(location.as_deref(), Some("/users/3"));

    // Immediately visible in the list (insertion order: seed first).
    let (_, list, _) = http(&state, "GET", "/users", None).await;
    assert_eq!(list.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_post_bringing_own_id_is_kept() {
    let (state, _dir) = collection_state();
    let (status, body, _) = http(
        &state,
        "POST",
        "/users",
        Some(r#"{"id":50,"name":"Owned"}"#),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(body["id"], 50);
    // Next assigned id continues from the largest seen.
    let (status, body, _) = http(&state, "POST", "/users", Some(r#"{"name":"Next"}"#)).await;
    assert_eq!(status, 201);
    assert_eq!(body["id"], 51);
}

#[tokio::test]
async fn test_put_replaces_patch_merges() {
    let (state, _dir) = collection_state();
    let (status, body, _) = http(
        &state,
        "PUT",
        "/users/2",
        Some(r#"{"name":"Grace Hopper","born":1906}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], 2, "PUT keeps the id");
    assert_eq!(body["name"], "Grace Hopper");

    let (status, body, _) = http(&state, "PATCH", "/users/2", Some(r#"{"known":"for"}"#)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["name"], "Grace Hopper",
        "PATCH merges, doesn't replace"
    );
    assert_eq!(body["known"], "for");

    // PUT cannot move the id.
    let (status, body, _) = http(
        &state,
        "PUT",
        "/users/2",
        Some(r#"{"id":9,"name":"Imposter"}"#),
    )
    .await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("id mismatch"));
}

#[tokio::test]
async fn test_delete_removes_row() {
    let (state, _dir) = collection_state();
    let (status, body, _) = http(&state, "DELETE", "/users/1", None).await;
    assert_eq!(status, 204);
    assert!(body.is_null());

    let (status, _, _) = http(&state, "GET", "/users/1", None).await;
    assert_eq!(status, 404);
    let (_, list, _) = http(&state, "GET", "/users", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_unknown_method_answers_405_not_fallthrough() {
    let (state, _dir) = collection_state();
    let (status, body, _) = http(&state, "PUT", "/users", Some(r#"{"name":"x"}"#)).await;
    assert_eq!(status, 405);
    assert_eq!(body["allow"], "GET, POST");

    let (status, body, _) = http(&state, "POST", "/users/2", Some(r#"{}"#)).await;
    assert_eq!(status, 405);
    assert_eq!(body["allow"], "GET, PUT, PATCH, DELETE");
}

#[tokio::test]
async fn test_unowned_paths_fall_through_to_mock_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("c.json"),
        r#"{"method":"GET","path":"/ignored","collection":{"name":"users","path":"/users","seed":[]}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("m.json"),
        r#"{"method":"GET","path":"/users/1/posts","status":200,"response":{"nested":true}}"#,
    )
    .unwrap();
    let state = handler::AppState::new(loader::load_mocks(dir.path().to_str().unwrap()))
        .with_mocks_dir(dir.path().to_str().unwrap().to_string());

    // A deeper path is NOT the collection's.
    let (status, body, _) = http(&state, "GET", "/users/1/posts", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["nested"], true);

    // Neither is a sibling prefix.
    let (status, body, _) = http(&state, "GET", "/users-admin", None).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], "mock not found");
}

#[tokio::test]
async fn test_admin_reset_returns_collections_to_seed() {
    let (state, _dir) = collection_state();
    http(&state, "POST", "/users", Some(r#"{"name":"Temp"}"#)).await;
    http(&state, "DELETE", "/users/1", None).await;

    let (status, report) = {
        let app = create_router(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        )
    };
    assert_eq!(status, 200);
    assert_eq!(report["reset_collections"], 1);

    // Two seed rows, exactly as declared; created id counter back at 2.
    let (_, list, _) = http(&state, "GET", "/users", None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (status, body, _) = http(&state, "POST", "/users", Some(r#"{"name":"Next"}"#)).await;
    assert_eq!(status, 201);
    assert_eq!(body["id"], 3, "reset rewinds the counter to the seed max");
}

#[tokio::test]
async fn test_collection_survives_hot_reload_and_stays_stateful() {
    let (state, dir) = collection_state();
    http(&state, "POST", "/users", Some(r#"{"name":"Created"}"#)).await;

    // One reload cycle, exactly as the reload path does.
    let result = loader::load_mocks_map(dir.path().to_str().unwrap());
    {
        let runtime = state.runtime_mocks.read().await;
        let runtime = runtime.clone();
        let mut store = state.mocks.write().await;
        *store = apply_reload(&store, result, &runtime);
        let live = live_identities(&store);
        prune_stale_state(&state.sequence_counters, &state.mock_hits, &live).await;
    }

    // Rows survive a reload (state is not re-seeded) — but reset does re-seed,
    // which the other test covers.
    let (_, list, _) = http(&state, "GET", "/users", None).await;
    assert_eq!(list.as_array().unwrap().len(), 3, "{list}");

    // And a config change (seed edit) takes effect after reset.
    std::fs::write(
        dir.path().join("users.json"),
        r#"{"method":"GET","path":"/ignored","collection":{
            "name": "users", "path": "/users", "id_field": "id",
            "seed": [{"id":1,"name":"Ada"},{"id":2,"name":"Grace"},{"id":3,"name":"NewSeed"}]
        }}"#,
    )
    .unwrap();
    let result = loader::load_mocks_map(dir.path().to_str().unwrap());
    {
        let runtime = state.runtime_mocks.read().await;
        let runtime = runtime.clone();
        let mut store = state.mocks.write().await;
        *store = apply_reload(&store, result, &runtime);
        let live = live_identities(&store);
        prune_stale_state(&state.sequence_counters, &state.mock_hits, &live).await;
    }
    let (status, report) = {
        let app = create_router(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes)
    };
    assert_eq!(status, 200);
    let (_, list, _) = http(&state, "GET", "/users", None).await;
    assert_eq!(list.as_array().unwrap().len(), 3);
    let report: serde_json::Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(report["reset_collections"], 1);
}

#[tokio::test]
async fn test_loader_rejects_collection_with_mock_fields() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("bad.json"),
        r#"{"method":"GET","path":"/x","status":200,"response":{"nope":true},
            "collection":{"name":"users","path":"/users","seed":[]}}"#,
    )
    .unwrap();
    let result = loader::load_mocks_map(dir.path().to_str().unwrap());
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0]
            .message
            .contains("declares a collection and a `response`"),
        "{:?}",
        result.errors[0].message
    );
}

#[tokio::test]
async fn collection_operation_overrides_validate_and_shape_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("users.json"),
        r#"{
        "method":"GET", "path":"/users",
        "collection":{"path":"/users","name":"users","operations":{
            "create":{"required_fields":["name"],"status":202,"response":{"accepted":true}},
            "delete":{"status":200,"response":{"deleted":true}}
        }}
    }"#,
    )
    .unwrap();
    let state = handler::AppState::new(loader::load_mocks(dir.path().to_str().unwrap()));
    let (status, error, _) = http(&state, "POST", "/users", Some("{}")).await;
    assert_eq!(status, 400);
    assert_eq!(error["detail"], "missing required field: name");
    let (status, body, location) = http(
        &state,
        "POST",
        "/users",
        Some(r#"{"id":"a/b","name":"Ada"}"#),
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(body, serde_json::json!({"accepted":true}));
    let location = location.unwrap();
    assert_eq!(location, "/users/a%2Fb");
    let (status, stored, _) = http(&state, "GET", &location, None).await;
    assert_eq!(status, 200);
    assert_eq!(stored["name"], "Ada");
    let (status, body, _) = http(&state, "DELETE", &location, None).await;
    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({"deleted":true}));
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
        let (status, body, _) = http(&state, method, &location, None).await;
        assert_eq!(status, 404);
        assert_eq!(body["collection"], "users");
    }
}
