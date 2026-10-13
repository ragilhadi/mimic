use super::*;
use crate::types::{BodyMatcher, MockConfig};

fn mock(method: &str, path: &str, marker: &str, tags: &[&str]) -> MockConfig {
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
        source_index: None,
        sequence: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

fn store(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for m in mocks {
        let key = crate::types::create_mock_key(&m.method, &m.path);
        map.entry(key).or_default().push(m);
    }
    map
}

fn active(tags: &[&str]) -> HashSet<String> {
    tags.iter().map(|t| t.to_string()).collect()
}

fn ctx(method: &str, path: &str) -> RequestContext {
    RequestContext::new(method.to_string(), path.to_string())
}

#[test]
fn untagged_mocks_match_regardless_of_the_active_scenario() {
    let mocks = store(vec![mock("GET", "/users", "plain", &[])]);

    for tags in [None, Some(active(&["anything"])), Some(HashSet::new())] {
        let result = find_matching_mock(&ctx("GET", "/users"), &mocks, tags.as_ref())
            .expect("untagged mocks are always matchable");
        assert_eq!(result.mock.response["source"], "plain");
    }
}

#[test]
fn a_tagged_mock_matches_only_while_one_of_its_tags_is_active() {
    let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);

    // No filter at all: tags are inert, exactly as before this feature.
    assert!(find_matching_mock(&ctx("POST", "/checkout"), &mocks, None).is_some());

    let on = active(&["error-scenario"]);
    assert!(find_matching_mock(&ctx("POST", "/checkout"), &mocks, Some(&on)).is_some());

    let off = active(&["happy-path"]);
    assert!(
        find_matching_mock(&ctx("POST", "/checkout"), &mocks, Some(&off)).is_none(),
        "an inactive mock must 404 as if it weren't loaded"
    );
}

#[test]
fn switching_tags_switches_which_mock_serves_a_path() {
    let mocks = store(vec![
        mock("POST", "/checkout", "ok", &["happy-path"]),
        mock("POST", "/checkout", "boom", &["error-scenario"]),
    ]);

    let happy = find_matching_mock(
        &ctx("POST", "/checkout"),
        &mocks,
        Some(&active(&["happy-path"])),
    )
    .expect("happy-path mock is active");
    assert_eq!(happy.mock.response["source"], "ok");
    assert_eq!(happy.index, 0);

    let error = find_matching_mock(
        &ctx("POST", "/checkout"),
        &mocks,
        Some(&active(&["error-scenario"])),
    )
    .expect("error-scenario mock is active");
    assert_eq!(error.mock.response["source"], "boom");
    // The index still points at the mock's real position in its bucket, so
    // sequence counters and hit counts stay keyed to the right mock.
    assert_eq!(error.index, 1);
}

#[test]
fn an_inactive_mock_falls_through_to_its_untagged_sibling() {
    let mocks = store(vec![
        mock("GET", "/users", "default", &[]),
        mock("GET", "/users", "chaos", &["error-scenario"]),
    ]);

    let result = find_matching_mock(
        &ctx("GET", "/users"),
        &mocks,
        Some(&active(&["happy-path"])),
    )
    .expect("the untagged mock is still matchable");
    assert_eq!(result.mock.response["source"], "default");
}

#[test]
fn tag_filtering_applies_to_pattern_routes_too() {
    let mocks = store(vec![mock(
        "GET",
        "/users/:id",
        "pattern",
        &["error-scenario"],
    )]);

    assert!(find_matching_mock(&ctx("GET", "/users/42"), &mocks, None).is_some());
    assert!(find_matching_mock(
        &ctx("GET", "/users/42"),
        &mocks,
        Some(&active(&["error-scenario"]))
    )
    .is_some());
    assert!(find_matching_mock(
        &ctx("GET", "/users/42"),
        &mocks,
        Some(&active(&["happy-path"]))
    )
    .is_none());
}

#[test]
fn an_inactive_exact_mock_falls_through_to_an_active_pattern_route() {
    let mocks = store(vec![
        mock("GET", "/users/42", "exact", &["error-scenario"]),
        mock("GET", "/users/:id", "pattern", &[]),
    ]);

    let result = find_matching_mock(
        &ctx("GET", "/users/42"),
        &mocks,
        Some(&active(&["happy-path"])),
    )
    .expect("the pattern route should serve the request");
    assert_eq!(result.mock.response["source"], "pattern");
    assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
}

#[test]
fn requires_body_ignores_mocks_the_scenario_hides() {
    let mut hungry = mock("POST", "/checkout", "boom", &["error-scenario"]);
    hungry.body = Some(BodyMatcher::Any);
    let mocks = store(vec![mock("POST", "/checkout", "ok", &[]), hungry]);

    // With the tagged mock active, its body matcher forces a body read.
    assert!(requires_body(
        "POST",
        "/checkout",
        &mocks,
        Some(&active(&["error-scenario"]))
    ));

    // Hidden by the scenario, it has no say — the untagged mock left
    // standing doesn't want the body, so `consume_body: false` holds.
    assert!(!requires_body(
        "POST",
        "/checkout",
        &mocks,
        Some(&active(&["happy-path"]))
    ));
}

#[test]
fn requires_body_reads_the_body_when_every_candidate_is_hidden() {
    // Nothing matchable is left, so the request is heading for a 404 and
    // its body is worth capturing for the request log.
    let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);
    assert!(requires_body(
        "POST",
        "/checkout",
        &mocks,
        Some(&active(&["happy-path"]))
    ));
}

#[test]
fn explain_no_match_names_the_scenario_as_the_reason() {
    let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);

    let explanation = explain_no_match(
        &ctx("POST", "/checkout"),
        &mocks,
        Some(&active(&["happy-path"])),
    )
    .expect("a filtered-out mock is worth explaining");

    assert!(
        explanation.contains("1 mock(s) match") && explanation.contains("inactive tags"),
        "unhelpful explanation: {}",
        explanation
    );
}

#[test]
fn explain_no_match_ignores_hidden_mocks_that_would_not_have_matched_anyway() {
    // Right path, wrong method: the tagged mock is hidden *and* irrelevant,
    // so the explanation should stay the "wrong method" one.
    let mocks = store(vec![mock("POST", "/checkout", "boom", &["error-scenario"])]);

    let explanation = explain_no_match(
        &ctx("GET", "/checkout"),
        &mocks,
        Some(&active(&["happy-path"])),
    )
    .expect("an unregistered route is always explained");

    assert!(!explanation.contains("inactive tags"), "{}", explanation);
    assert!(explanation.contains("POST"), "{}", explanation);
}
