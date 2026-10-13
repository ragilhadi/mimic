use super::*;
use crate::types::{MockConfig, SequenceStep};
use axum::http::Method;
use http_body_util::BodyExt;

fn mock(method: &str, path: &str, marker: &str, tags: &[&str]) -> MockConfig {
    MockConfig {
        method: method.to_string(),
        path: path.to_string(),
        status: 200,
        response: json!({"source": marker}),
        consume_body: false,
        query_params: None,
        headers: None,
        body: None,
        delay_ms: None,
        response_headers: None,
        source: None,
        source_index: None,
        sequence: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

fn state_with(mocks: Vec<MockConfig>, active: Option<&[&str]>) -> AppState {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for m in mocks {
        let key = crate::types::create_mock_key(&m.method, &m.path);
        map.entry(key).or_default().push(m);
    }
    AppState::with_active_tags(
        Arc::new(tokio::sync::RwLock::new(map)),
        active.map(|tags| tags.iter().map(|t| t.to_string()).collect()),
    )
}

/// Two mocks on one path, told apart by scenario — the setup from the
/// issue: a happy-path checkout and its 500 counterpart.
fn checkout_state(active: Option<&[&str]>) -> AppState {
    state_with(
        vec![
            mock("POST", "/checkout", "ok", &["happy-path"]),
            mock("POST", "/checkout", "boom", &["error-scenario"]),
        ],
        active,
    )
}

async fn get(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
    request(state, Method::GET, path).await
}

async fn request(state: &AppState, method: Method, path: &str) -> (StatusCode, serde_json::Value) {
    let response = handle_request(
        method,
        path.parse().unwrap(),
        HeaderMap::new(),
        State(state.clone()),
        Body::empty(),
    )
    .await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, serde_json::from_slice(&bytes).unwrap())
}

async fn post_scenario(state: &AppState, body: &str) -> (StatusCode, serde_json::Value) {
    let response = set_scenario(State(state.clone()), Bytes::from(body.to_string())).await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, serde_json::from_slice(&bytes).unwrap())
}

// ── GET /admin/scenario ─────────────────────────────────────────────

#[tokio::test]
async fn scenario_defaults_to_no_filtering() {
    let state = checkout_state(None);
    let Json(body) = get_scenario(State(state)).await;

    assert_eq!(body["filtering"], false);
    assert_eq!(body["active_tags"], json!([]));
    assert_eq!(body["known_tags"], json!(["error-scenario", "happy-path"]));
    assert_eq!(body["matchable_mocks"], 2);
    assert_eq!(body["total_mocks"], 2);
}

#[tokio::test]
async fn scenario_reports_the_startup_selection() {
    let state = checkout_state(Some(&["happy-path"]));
    let Json(body) = get_scenario(State(state)).await;

    assert_eq!(body["filtering"], true);
    assert_eq!(body["active_tags"], json!(["happy-path"]));
    assert_eq!(body["matchable_mocks"], 1);
}

// ── POST /admin/scenario ────────────────────────────────────────────

#[tokio::test]
async fn posting_tags_replaces_the_active_set() {
    let state = checkout_state(Some(&["happy-path"]));

    let (status, body) = post_scenario(&state, r#"{"tags": ["error-scenario"]}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active_tags"], json!(["error-scenario"]));

    // A follow-up GET agrees with what the POST reported.
    let Json(after) = get_scenario(State(state)).await;
    assert_eq!(after["active_tags"], json!(["error-scenario"]));
}

#[tokio::test]
async fn posting_an_empty_list_clears_the_filter() {
    let state = checkout_state(Some(&["happy-path"]));

    let (status, body) = post_scenario(&state, r#"{"tags": []}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["filtering"], false);
    assert_eq!(body["matchable_mocks"], 2);
}

#[tokio::test]
async fn posting_a_comma_separated_tag_works_like_the_env_var() {
    let state = checkout_state(None);

    let (_, body) = post_scenario(&state, r#"{"tags": ["happy-path, error-scenario"]}"#).await;
    assert_eq!(body["active_tags"], json!(["error-scenario", "happy-path"]));
}

#[tokio::test]
async fn posting_malformed_json_is_a_400_and_leaves_the_scenario_alone() {
    let state = checkout_state(Some(&["happy-path"]));

    let (status, body) = post_scenario(&state, "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid scenario request");

    let Json(after) = get_scenario(State(state)).await;
    assert_eq!(after["active_tags"], json!(["happy-path"]));
}

// ── End to end: switching scenarios changes what is served ──────────

#[tokio::test]
async fn switching_scenarios_switches_the_served_mock_without_a_restart() {
    let state = checkout_state(Some(&["happy-path"]));

    let (status, body) = request(&state, Method::POST, "/checkout").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "ok");

    post_scenario(&state, r#"{"tags": ["error-scenario"]}"#).await;

    let (status, body) = request(&state, Method::POST, "/checkout").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "boom");
}

#[tokio::test]
async fn an_inactive_mock_404s_as_if_it_were_not_loaded() {
    let state = state_with(
        vec![mock("POST", "/checkout", "boom", &["error-scenario"])],
        Some(&["happy-path"]),
    );

    let (status, body) = request(&state, Method::POST, "/checkout").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "mock not found");
    // The 404 body is the same one an unregistered path gets: scenario
    // configuration is not leaked to API clients.
    assert!(body.get("tags").is_none());
    assert!(!body.to_string().contains("error-scenario"));

    // The reason is available to operators through the request log.
    let log = state.request_log.read().await;
    let explanation = log[0].match_explanation.as_deref().unwrap_or_default();
    assert!(explanation.contains("inactive tags"), "{}", explanation);
}

#[tokio::test]
async fn untagged_mocks_are_unaffected_by_any_scenario() {
    let state = state_with(vec![mock("GET", "/users", "plain", &[])], Some(&["chaos"]));

    let (status, body) = get(&state, "/users").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "plain");

    post_scenario(&state, r#"{"tags": ["anything-else"]}"#).await;

    let (status, body) = get(&state, "/users").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "plain");
}

// ── Interaction with other state ────────────────────────────────────

#[tokio::test]
async fn switching_scenarios_does_not_reset_sequence_counters() {
    let mut sequenced = mock("GET", "/poll", "seq", &["happy-path"]);
    sequenced.sequence = Some(vec![
        SequenceStep {
            status: 200,
            response: json!({"n": 1}),
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
            repeat: false,
        },
        SequenceStep {
            status: 200,
            response: json!({"n": 2}),
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
            repeat: false,
        },
        SequenceStep {
            status: 200,
            response: json!({"n": 3}),
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
            repeat: true,
        },
    ]);
    let state = state_with(vec![sequenced], Some(&["happy-path"]));

    assert_eq!(get(&state, "/poll").await.1["n"], 1);
    assert_eq!(get(&state, "/poll").await.1["n"], 2);

    // Switch away (the mock stops matching), then back.
    post_scenario(&state, r#"{"tags": ["error-scenario"]}"#).await;
    assert_eq!(
        get(&state, "/poll").await.0,
        StatusCode::NOT_FOUND,
        "the sequenced mock is inactive while the scenario is switched away"
    );
    post_scenario(&state, r#"{"tags": ["happy-path"]}"#).await;

    // The counter picked up where it left off rather than restarting.
    assert_eq!(get(&state, "/poll").await.1["n"], 3);
}

#[tokio::test]
async fn hit_counts_stay_keyed_to_the_right_mock_across_a_switch() {
    let state = checkout_state(Some(&["error-scenario"]));

    request(&state, Method::POST, "/checkout").await;

    let hits = state.mock_hits.read().await;
    // These mocks are built in-process with no `source`, so their identity
    // falls back to bucket position. Index 1 is the error-scenario mock's
    // real position; filtering must not renumber the mocks around it.
    let at = |index: usize| MockIdentity {
        key: "POST:/checkout".to_string(),
        origin: crate::types::MockOrigin::Position(index),
    };
    assert_eq!(hits.get(&at(1)).copied(), Some(1));
    assert_eq!(hits.get(&at(0)).copied(), None);
}

#[tokio::test]
async fn admin_mocks_reports_tags_and_whether_each_mock_is_active() {
    let state = checkout_state(Some(&["happy-path"]));
    let Json(body) = list_mocks(State(state)).await;

    let entries = body["mocks"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["tags"], json!(["happy-path"]));
    assert_eq!(entries[0]["active"], true);
    assert_eq!(entries[1]["tags"], json!(["error-scenario"]));
    assert_eq!(entries[1]["active"], false);
}

// ── MIMIC_ACTIVE_TAGS parsing ───────────────────────────────────────

#[test]
fn configured_active_tags_reads_the_env_var() {
    // Serialized by the env var itself: this is the only test that touches
    // MIMIC_ACTIVE_TAGS, so the mutation can't race another test.
    let restore = std::env::var(ACTIVE_TAGS_ENV).ok();

    std::env::set_var(ACTIVE_TAGS_ENV, "happy-path, smoke-test");
    assert_eq!(
        configured_active_tags(),
        Some(
            ["happy-path", "smoke-test"]
                .iter()
                .map(|t| t.to_string())
                .collect::<HashSet<String>>()
        )
    );

    std::env::set_var(ACTIVE_TAGS_ENV, "");
    assert_eq!(configured_active_tags(), None);

    std::env::remove_var(ACTIVE_TAGS_ENV);
    assert_eq!(configured_active_tags(), None);

    if let Some(value) = restore {
        std::env::set_var(ACTIVE_TAGS_ENV, value);
    }
}

// ── MIMIC_BIND_ADDRESS parsing (#114) ───────────────────────────────

#[test]
fn parse_bind_address_defaults_to_unspecified_when_unset() {
    assert_eq!(
        parse_bind_address(None),
        Ok(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    );
}

#[test]
fn parse_port_defaults_when_unset_or_empty_and_rejects_garbage() {
    assert_eq!(parse_port(None), Ok(8080));
    assert_eq!(parse_port(Some("".to_string())), Ok(8080));
    assert_eq!(parse_port(Some("  ".to_string())), Ok(8080));
    assert_eq!(parse_port(Some(" 9090 ".to_string())), Ok(9090));
    // Previously an unparsable PORT silently became 8080 while an
    // unparsable MIMIC_BIND_ADDRESS aborted; both abort now.
    for bad in ["abc", "70000", "-1", "80.5"] {
        let err = parse_port(Some(bad.to_string())).unwrap_err();
        assert!(err.contains("PORT") && err.contains(bad), "{err}");
    }
}

#[test]
fn parse_bind_address_accepts_ipv4_and_ipv6_literals() {
    assert_eq!(
        parse_bind_address(Some("127.0.0.1".to_string())),
        Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    );
    assert_eq!(
        parse_bind_address(Some("::1".to_string())),
        Ok(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
    );
    assert_eq!(
        parse_bind_address(Some("::".to_string())),
        Ok(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED))
    );
}

#[test]
fn parse_bind_address_rejects_an_unparsable_value_instead_of_falling_back() {
    // The whole point of the setting: a typo must fail startup rather than
    // silently binding wider than asked.
    let err = parse_bind_address(Some("not-an-address".to_string())).unwrap_err();
    assert!(err.contains(BIND_ADDRESS_ENV));
    assert!(err.contains("not-an-address"));
}

#[test]
fn parse_bind_address_rejects_a_socket_address_with_a_port() {
    // A common mistake — the setting takes a bare address, not host:port.
    assert!(parse_bind_address(Some("127.0.0.1:8080".to_string())).is_err());
}

#[test]
fn configured_bind_address_reads_the_env_var() {
    // Serialized by the env var itself: this is the only test that touches
    // MIMIC_BIND_ADDRESS, so the mutation can't race another test.
    let restore = std::env::var(BIND_ADDRESS_ENV).ok();

    std::env::set_var(BIND_ADDRESS_ENV, "127.0.0.1");
    assert_eq!(
        configured_bind_address(),
        Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    );

    std::env::remove_var(BIND_ADDRESS_ENV);
    assert_eq!(
        configured_bind_address(),
        Ok(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    );

    std::env::set_var(BIND_ADDRESS_ENV, "nope");
    assert!(configured_bind_address().is_err());

    match restore {
        Some(value) => std::env::set_var(BIND_ADDRESS_ENV, value),
        None => std::env::remove_var(BIND_ADDRESS_ENV),
    }
}

// ── Reserved routes (#87) ───────────────────────────────────────────

fn reserved(health: Option<&str>, admin: Option<&str>, disable: Option<&str>) -> ReservedRoutes {
    ReservedRoutes::from_values(
        health.map(str::to_string),
        admin.map(str::to_string),
        disable.map(str::to_string),
    )
}

#[test]
fn unset_variables_reserve_exactly_what_mimic_has_always_reserved() {
    assert_eq!(reserved(None, None, None), ReservedRoutes::default());
    assert_eq!(
        ReservedRoutes::default().health.as_deref(),
        Some(DEFAULT_HEALTH_PATH)
    );
    assert_eq!(
        ReservedRoutes::default().admin_prefix.as_deref(),
        Some(DEFAULT_ADMIN_PREFIX)
    );
}

#[test]
fn only_the_exact_pairs_mimic_answers_are_reserved() {
    let routes = ReservedRoutes::default();

    assert!(routes.reservation_for("GET", "/health").is_some());
    assert!(routes.reservation_for("GET", "/admin/mocks").is_some());
    assert!(routes.reservation_for("POST", "/admin/scenario").is_some());
    assert!(routes
        .reservation_for("DELETE", "/admin/requests")
        .is_some());

    // These reach the fallback and are perfectly good mocks; warning about
    // them would be noise, and marking them unreachable would be a lie.
    assert_eq!(routes.reservation_for("POST", "/health"), None);
    assert_eq!(routes.reservation_for("POST", "/admin/mocks"), None);
    assert_eq!(routes.reservation_for("GET", "/admin/users"), None);
    assert_eq!(routes.reservation_for("GET", "/healthz"), None);
}

#[test]
fn method_comparison_is_case_insensitive() {
    let routes = ReservedRoutes::default();
    assert!(routes.reservation_for("get", "/health").is_some());
}

#[test]
fn an_empty_value_switches_a_route_off() {
    let routes = reserved(Some(""), None, None);
    assert_eq!(routes.health, None);
    assert_eq!(
        routes.reservation_for("GET", "/health"),
        None,
        "with the health check gone, the path is a mock's to claim"
    );
}

#[test]
fn the_admin_api_can_be_moved_or_switched_off() {
    let moved = reserved(None, Some("/_mimic"), None);
    assert!(moved.reservation_for("GET", "/_mimic/mocks").is_some());
    assert_eq!(moved.reservation_for("GET", "/admin/mocks"), None);

    for truthy in ["true", "1", "yes", "on", " TRUE "] {
        let off = reserved(None, None, Some(truthy));
        assert_eq!(off.admin_prefix, None, "MIMIC_DISABLE_ADMIN={}", truthy);
        assert_eq!(off.reservation_for("GET", "/admin/mocks"), None);
    }

    // A non-affirmative value leaves the admin API where it is, rather
    // than treating "any value at all" as "off".
    assert_eq!(
        reserved(None, None, Some("false")).admin_prefix.as_deref(),
        Some(DEFAULT_ADMIN_PREFIX)
    );
}

#[test]
fn a_prefix_is_normalized_so_it_can_never_panic_the_router() {
    // `Router::route` panics on a path without a leading slash, and a
    // trailing slash would produce "/ops//mocks".
    assert_eq!(
        reserved(None, Some("  /ops/  "), None)
            .admin_prefix
            .as_deref(),
        Some("/ops")
    );
    assert_eq!(
        reserved(None, Some("ops"), None).admin_prefix.as_deref(),
        Some("/ops")
    );
    assert_eq!(reserved(None, Some("/"), None).admin_prefix, None);
}

#[test]
fn disable_wins_over_a_configured_prefix() {
    assert_eq!(
        reserved(None, Some("/ops"), Some("true")).admin_prefix,
        None
    );
}

#[tokio::test]
async fn admin_mocks_marks_a_shadowed_mock_unreachable() {
    // The reported case: a mock for GET /health loads, is listed, and can
    // never serve. `hits: 0` alone doesn't say which of those it is.
    let state = state_with(
        vec![
            mock("GET", "/health", "health-down", &[]),
            mock("GET", "/users", "users", &[]),
        ],
        None,
    );

    let Json(body) = list_mocks(State(state)).await;
    let entries = body["mocks"].as_array().unwrap();

    let health = entries
        .iter()
        .find(|e| e["path"] == "/health")
        .expect("the shadowed mock is still listed");
    assert_eq!(health["reachable"], false);
    assert!(health["unreachable_reason"]
        .as_str()
        .unwrap()
        .contains("reserved by"));

    let users = entries.iter().find(|e| e["path"] == "/users").unwrap();
    assert_eq!(users["reachable"], true);
    assert_eq!(users["unreachable_reason"], serde_json::Value::Null);
}

// ── Out-of-range status (#104) ──────────────────────────────────────

#[tokio::test]
async fn admin_mocks_marks_an_out_of_range_status_unservable() {
    let mut bad = mock("GET", "/bad", "bad", &[]);
    bad.status = 1000;
    let state = state_with(vec![bad, mock("GET", "/users", "users", &[])], None);

    let Json(body) = list_mocks(State(state)).await;
    let entries = body["mocks"].as_array().unwrap();

    let bad_entry = entries.iter().find(|e| e["path"] == "/bad").unwrap();
    assert_eq!(bad_entry["servable"], false);
    assert!(bad_entry["unservable_reason"]
        .as_str()
        .unwrap()
        .contains("outside 100-599"));

    let users = entries.iter().find(|e| e["path"] == "/users").unwrap();
    assert_eq!(users["servable"], true);
    assert_eq!(users["unservable_reason"], serde_json::Value::Null);
}

/// The request log must record what was actually put on the wire, not the
/// raw configured number — for every shape of "invalid" the repro named.
#[tokio::test]
async fn response_and_log_agree_for_every_out_of_range_status() {
    for bad_status in [0u16, 99, 600, 1000] {
        let mut bad = mock("GET", "/bad", "bad", &[]);
        bad.status = bad_status;
        let state = state_with(vec![bad], None);

        let response = handle_request(
            Method::GET,
            "/bad".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let served_status = response.status().as_u16();

        let log = state.request_log.read().await;
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].response_status, served_status,
            "status {} in the mock: log and response disagreed",
            bad_status
        );
    }
}

// ── Body redaction (#88) ────────────────────────────────────────────

fn policy(fields: &[&str]) -> BodyRedaction {
    BodyRedaction {
        fields: fields.iter().map(|f| f.to_string()).collect(),
        disabled: false,
    }
}

fn redact(body: &str, content_type: Option<&str>) -> String {
    redact_body(body, content_type, &BodyRedaction::default().fields)
}

#[test]
fn the_default_policy_scrubs_a_password_out_of_a_json_body() {
    // The reported case: the credential sitting next to a header that was
    // already redacted.
    let stored = redact(
        r#"{"username":"alice","password":"hunter2"}"#,
        Some("application/json"),
    );
    let json: serde_json::Value = serde_json::from_str(&stored).unwrap();

    assert_eq!(json["password"], REDACTED);
    assert_eq!(json["username"], "alice", "only the secret is touched");
}

#[test]
fn redaction_reaches_nested_objects_and_arrays() {
    let stored = redact(
        r#"{"users":[{"name":"a","token":"t1"},{"name":"b","token":"t2"}],
            "auth":{"nested":{"api_key":"k"}}}"#,
        Some("application/json"),
    );
    let json: serde_json::Value = serde_json::from_str(&stored).unwrap();

    assert_eq!(json["users"][0]["token"], REDACTED);
    assert_eq!(json["users"][1]["token"], REDACTED);
    assert_eq!(json["auth"]["nested"]["api_key"], REDACTED);
    assert_eq!(json["users"][0]["name"], "a");
}

#[test]
fn a_matching_key_loses_its_whole_subtree() {
    // `{"token": {"value": "..."}}` must not leak because the secret is
    // one level further in than the name that identified it.
    let stored = redact(
        r#"{"token":{"value":"secret","expires":1}}"#,
        Some("application/json"),
    );
    let json: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(json["token"], REDACTED);
}

#[test]
fn field_names_are_matched_case_insensitively_and_exactly() {
    let stored = redact(
        r#"{"Password":"x","PASSWORD":"y","tokenizer":"keep me"}"#,
        Some("application/json"),
    );
    let json: serde_json::Value = serde_json::from_str(&stored).unwrap();

    assert_eq!(json["Password"], REDACTED);
    assert_eq!(json["PASSWORD"], REDACTED);
    assert_eq!(
        json["tokenizer"], "keep me",
        "exact matching: `token` must not swallow `tokenizer`"
    );
}

#[test]
fn form_bodies_are_redacted_field_wise() {
    let stored = redact(
        "username=alice&password=hunter2&remember=1",
        Some("application/x-www-form-urlencoded"),
    );
    assert_eq!(
        stored,
        format!("username=alice&password={}&remember=1", REDACTED)
    );
}

#[test]
fn a_body_with_nothing_to_redact_is_stored_byte_for_byte() {
    // Reserializing a clean body would rewrite its whitespace, making the
    // log a worse record of what was actually sent.
    let original = "{\n  \"username\": \"alice\"\n}";
    assert_eq!(redact(original, Some("application/json")), original);

    let text = "just some plain text with the word password in it";
    assert_eq!(redact(text, Some("text/plain")), text);
}

#[test]
fn an_empty_field_list_stores_bodies_verbatim() {
    // The documented escape hatch: MIMIC_REDACT_BODY_FIELDS= restores the
    // old behavior for anyone who wants it.
    let verbatim = BodyRedaction::from_values(Some(String::new()), None);
    assert!(verbatim.fields.is_empty());
    assert_eq!(
        verbatim.apply(r#"{"password":"hunter2"}"#.to_string(), None),
        Some(r#"{"password":"hunter2"}"#.to_string())
    );
}

#[test]
fn an_unset_variable_uses_the_default_list_rather_than_nothing() {
    let default = BodyRedaction::from_values(None, None);
    assert_eq!(default, BodyRedaction::default());
    assert!(default.fields.contains("password"));
}

#[test]
fn a_configured_list_replaces_the_default_and_is_trimmed() {
    let configured = BodyRedaction::from_values(Some(" PIN , cvv ,, ".to_string()), None);
    let expected: HashSet<String> = ["pin", "cvv"].iter().map(|f| f.to_string()).collect();
    assert_eq!(configured.fields, expected);
    assert!(
        !configured.fields.contains("password"),
        "an explicit list is the whole list, not an addition to the default"
    );
}

#[test]
fn disabling_the_body_log_stores_nothing() {
    let off = BodyRedaction::from_values(None, Some("true".to_string()));
    assert!(off.disabled);
    assert_eq!(off.apply("anything at all".to_string(), None), None);
}

#[tokio::test]
async fn a_recorded_request_and_response_are_both_redacted() {
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    let mut login = mock("POST", "/login", "login", &[]);
    login.response = json!({"access_token": "very-secret", "user": "alice"});
    login.consume_body = true;
    mocks.insert("POST:/login".to_string(), vec![login]);
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("authorization", "Bearer super-secret".parse().unwrap());
    handle_request(
        Method::POST,
        "/login".parse().unwrap(),
        headers,
        State(state.clone()),
        Body::from(r#"{"username":"alice","password":"hunter2"}"#),
    )
    .await;

    let log = state.request_log.read().await;
    let record = &log[0];

    let body: serde_json::Value = serde_json::from_str(record.body.as_ref().unwrap()).unwrap();
    assert_eq!(body["password"], REDACTED);
    assert_eq!(body["username"], "alice");

    // A mock returning a token has the same problem the request did.
    let response: serde_json::Value =
        serde_json::from_str(record.response_body.as_ref().unwrap()).unwrap();
    assert_eq!(response["access_token"], REDACTED);
    assert_eq!(response["user"], "alice");

    assert_eq!(record.headers["authorization"], REDACTED);
}

#[tokio::test]
async fn redaction_does_not_change_what_the_client_receives() {
    // Redaction is a property of the log, not of the mock server: the
    // client must still get the token the mock promised.
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    let mut login = mock("POST", "/login", "login", &[]);
    login.response = json!({"access_token": "very-secret"});
    mocks.insert("POST:/login".to_string(), vec![login]);
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    let response = handle_request(
        Method::POST,
        "/login".parse().unwrap(),
        HeaderMap::new(),
        State(state),
        Body::empty(),
    )
    .await;

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["access_token"], "very-secret");
}

#[tokio::test]
async fn a_matcher_still_sees_the_real_body() {
    // Matching runs before recording and against the unredacted body, so
    // a mock keyed on a password field keeps working.
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    let mut login = mock("POST", "/login", "matched", &[]);
    login.consume_body = true;
    login.body = Some(crate::types::BodyMatcher::Json(
        crate::types::JsonBodyMatcher {
            exact: None,
            partial: Some(json!({"password": "hunter2"})),
            strict: false,
        },
    ));
    mocks.insert("POST:/login".to_string(), vec![login]);
    let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    let response = handle_request(
        Method::POST,
        "/login".parse().unwrap(),
        headers,
        State(state.clone()),
        Body::from(r#"{"password":"hunter2"}"#),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    // …and the log still doesn't keep it.
    let log = state.request_log.read().await;
    assert!(!log[0].body.as_ref().unwrap().contains("hunter2"));
}

#[tokio::test]
async fn a_custom_policy_is_honored_end_to_end() {
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    let mut pay = mock("POST", "/pay", "pay", &[]);
    pay.consume_body = true;
    mocks.insert("POST:/pay".to_string(), vec![pay]);
    let state =
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks))).with_redaction(policy(&["cvv"]));

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    handle_request(
        Method::POST,
        "/pay".parse().unwrap(),
        headers,
        State(state.clone()),
        Body::from(r#"{"cvv":"123","password":"hunter2"}"#),
    )
    .await;

    let log = state.request_log.read().await;
    let body: serde_json::Value = serde_json::from_str(log[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["cvv"], REDACTED);
    assert_eq!(
        body["password"], "hunter2",
        "a configured list replaces the default entirely"
    );
}

// ── Admin API authentication (#88) ──────────────────────────────────

#[test]
fn the_token_guards_the_admin_endpoints_and_not_the_health_check() {
    let routes = ReservedRoutes::default();

    assert!(routes.is_admin_endpoint("GET", "/admin/requests"));
    assert!(routes.is_admin_endpoint("DELETE", "/admin/requests"));
    assert!(routes.is_admin_endpoint("GET", "/admin/dashboard"));

    // Liveness probes call this and carry no credentials.
    assert!(!routes.is_admin_endpoint("GET", "/health"));
    // Not an endpoint Mimic answers — an ordinary mock request.
    assert!(!routes.is_admin_endpoint("GET", "/admin/users"));
    assert!(!routes.is_admin_endpoint("PUT", "/admin/requests"));
}

#[test]
fn the_token_guards_every_data_endpoint_but_frees_the_dashboard_shell() {
    let routes = ReservedRoutes::default();

    for route in ["requests", "mocks", "sequences", "scenario"] {
        assert!(
            routes.is_token_guarded("GET", &format!("/admin/{route}")),
            "a data endpoint stays guarded: /admin/{route}"
        );
    }
    // The reset endpoint is POST-only, and POST is what it guards with.
    assert!(routes.is_token_guarded("POST", "/admin/sequences/reset"));
    assert!(
        !routes.is_token_guarded("GET", "/admin/sequences/reset"),
        "a GET isn't the reset endpoint at all"
    );

    // The shell must reach the browser before it can present a token
    // (#113) — it is a compiled-in constant, no data in it. It is still
    // an admin endpoint (a mock for it stays shadowed); it just isn't
    // the token's business.
    assert!(!routes.is_token_guarded("GET", "/admin/dashboard"));
    assert!(routes.is_admin_endpoint("GET", "/admin/dashboard"));

    // Unguarded on the same grounds the predicate above records.
    assert!(!routes.is_token_guarded("GET", "/health"));
    assert!(!routes.is_token_guarded("GET", "/admin/users"));
    assert!(!routes.is_token_guarded("PUT", "/admin/requests"));
}

#[test]
fn a_moved_admin_prefix_moves_the_shell_exemption_with_it() {
    let routes = ReservedRoutes::from_values(None, Some("/_mimic".to_string()), None);

    assert!(!routes.is_token_guarded("GET", "/_mimic/dashboard"));
    assert!(routes.is_token_guarded("GET", "/_mimic/requests"));
    assert_eq!(
        routes.reservation_for("GET", "/_mimic/dashboard"),
        Some("Mimic's admin API")
    );
}

#[test]
fn no_admin_prefix_guards_or_reserves_nothing() {
    let routes = ReservedRoutes::from_values(None, None, Some("true".to_string()));

    assert!(!routes.is_token_guarded("GET", "/admin/dashboard"));
    assert!(!routes.is_token_guarded("GET", "/admin/requests"));
}

#[tokio::test]
async fn a_mock_is_reachable_once_the_route_it_collided_with_is_freed() {
    let state = state_with(vec![mock("GET", "/health", "health-down", &[])], None)
        .with_reserved(reserved(Some(""), None, None));

    let Json(body) = list_mocks(State(state)).await;
    assert_eq!(body["mocks"][0]["reachable"], true);
}
