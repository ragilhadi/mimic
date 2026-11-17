use super::*;

// =====================================================================
// #147 — path wildcards, regex-constrained and optional parameters
// =====================================================================

fn params(template: &str, request: &str) -> Option<HashMap<String, String>> {
    let pattern = compile_path_pattern(template)?;
    match_path_pattern(&pattern, request)
}

fn assert_params(template: &str, request: &str, want: &[(&str, &str)]) {
    let got = params(template, request)
        .unwrap_or_else(|| panic!("template `{}` should match `{}`", template, request));
    let want: HashMap<String, String> = want
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(got, want, "template `{}` against `{}`", template, request);
}

fn assert_no_match(template: &str, request: &str) {
    assert!(
        params(template, request).is_none(),
        "template `{}` should NOT match `{}`",
        template,
        request
    );
}

// ------------------------------------------------------------------
// `*` — exactly one unnamed segment
// ------------------------------------------------------------------

#[test]
fn test_star_matches_exactly_one_segment() {
    assert_params("/api/*/users", "/api/v1/users", &[("*", "v1")]);
    assert_no_match("/api/*/users", "/api/users");
    assert_no_match("/api/*/users", "/api/v1/v2/users");
}

#[test]
fn test_star_is_pattern_path_but_scores_as_parameter() {
    assert!(is_pattern_path("/api/*/users"));
    // Specificity counts literals only: /api/*/users → api + users.
    assert_eq!(path_specificity("/api/*/users"), 2);
}

// ------------------------------------------------------------------
// `**` — one or more trailing segments, captured with slashes
// ------------------------------------------------------------------

#[test]
fn test_double_star_matches_trailing_segments() {
    assert_params("/files/**", "/files/a", &[("*", "a")]);
    assert_params("/files/**", "/files/a/b/c", &[("*", "a/b/c")]);
    assert_no_match("/files/**", "/files");
}

#[test]
fn test_named_double_star_spellings() {
    assert_params("/files/{rest**}", "/files/a/b", &[("rest", "a/b")]);
    assert_params("/files/:rest*", "/files/a/b", &[("rest", "a/b")]);
}

#[test]
fn test_double_star_in_the_middle_is_malformed() {
    // `**` not final: rejected at compile time.
    assert!(compile_path_pattern("/files/**/download").is_none());
    assert!(compile_path_pattern("/a/**/b/**").is_none());
}

#[test]
fn test_wildcard_slash_prefilter_is_disabled_but_matching_works() {
    // A request with a different slash count than the template must still
    // match a `**` template — the prefilter must not reject it.
    let pattern = compile_path_pattern("/assets/**").unwrap();
    assert!(pattern.slashes.is_none());
    assert!(match_path_pattern(&pattern, "/assets/js/app.js").is_some());
}

// ------------------------------------------------------------------
// regex-constrained parameters
// ------------------------------------------------------------------

#[test]
fn test_constrained_param_express_style() {
    assert_params("/users/:id(\\d+)", "/users/42", &[("id", "42")]);
    assert_no_match("/users/:id(\\d+)", "/users/abc");
    assert_no_match("/users/:id(\\d+)", "/users/4x2");
}

#[test]
fn test_constrained_param_openapi_style() {
    assert_params("/users/{id:\\d+}", "/users/42", &[("id", "42")]);
    assert_no_match("/users/{id:\\d+}", "/users/me");
}

#[test]
fn test_constrained_param_distinguishes_me_from_numeric_id() {
    // The motivation case: `/users/me` and `/users/:id(\d+)` no longer
    // collide — a numeric id matches the pattern, `me` does not.
    assert!(params("/users/:id(\\d+)", "/users/123").is_some());
    assert!(params("/users/:id(\\d+)", "/users/me").is_none());
}

#[test]
fn test_constraint_with_optional_quantifier_is_not_optional_marker() {
    // `(\d+?)` contains a `?` inside the constraint — it constrains, and the
    // capture reflects the regex's laziness.
    assert_params("/a/:id(\\d+?)", "/a/7", &[("id", "7")]);
}

#[test]
fn test_constraint_cannot_contain_own_capture_group_shifting_positions() {
    // A group inside the constraint is wrapped non-capturing, so positional
    // numbering stays stable: the segment is still capture group 1.
    assert_params("/v/:ver(v\\d+\\.\\d+)", "/v/v1.2", &[("ver", "v1.2")]);
}

// ------------------------------------------------------------------
// optional final parameters
// ------------------------------------------------------------------

#[test]
fn test_optional_param_matches_with_and_without_value() {
    assert_params("/v1/users/:id?", "/v1/users/42", &[("id", "42")]);
    assert_params("/v1/users/:id?", "/v1/users", &[]);
    assert_no_match("/v1/users/:id?", "/v1/users/42/extra");
}

#[test]
fn test_optional_param_braced_form() {
    assert_params("/v1/users/{id?}", "/v1/users/42", &[("id", "42")]);
    assert_params("/v1/users/{id?}", "/v1/users", &[]);
}

#[test]
fn test_optional_param_not_final_is_malformed() {
    assert!(compile_path_pattern("/v1/:id?/users").is_none());
}

#[test]
fn test_optional_param_slash_prefilter_is_disabled() {
    let pattern = compile_path_pattern("/v1/users/:id?").unwrap();
    assert!(pattern.slashes.is_none());
    // Both widths match.
    assert!(match_path_pattern(&pattern, "/v1/users").is_some());
    assert!(match_path_pattern(&pattern, "/v1/users/9").is_some());
}

// ------------------------------------------------------------------
// classification and specificity
// ------------------------------------------------------------------

#[test]
fn test_is_pattern_path_recognizes_new_syntax() {
    assert!(is_pattern_path("/assets/**"));
    assert!(is_pattern_path("/api/*"));
    assert!(is_pattern_path("/users/:id(\\d+)"));
    assert!(is_pattern_path("/users/{id:\\d+}"));
    assert!(is_pattern_path("/users/:id?"));
    // Literal paths with no dynamic segment stay non-patterns.
    assert!(!is_pattern_path("/users/list"));
    assert!(!is_pattern_path("/a*x")); // plain literal containing a star in text
    assert!(!is_pattern_path("/legacy:port")); // colon inside a literal segment text
}

#[test]
fn test_path_specificity_ranks_new_kinds() {
    // More literal segments = more specific.
    assert_eq!(path_specificity("/users/:id"), 1);
    assert_eq!(path_specificity("/users/42"), 2);
    assert_eq!(path_specificity("/assets/**"), 1);
    assert_eq!(path_specificity("/:any/**"), 0);
    assert_eq!(path_specificity("/api/*/users"), 2);
}

// ------------------------------------------------------------------
// integration through find_matching_mock
// ------------------------------------------------------------------

fn mock(method: &str, path: &str) -> MockConfig {
    MockConfig {
        method: method.to_string(),
        path: path.to_string(),
        status: 200,
        ..Default::default()
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

fn context(method: &str, path: &str) -> RequestContext {
    let mut ctx = RequestContext::new(method.to_string(), path.to_string());
    ctx.body = None;
    ctx
}

#[test]
fn test_static_file_wildcard_route_serves() {
    let mocks = store(vec![mock("GET", "/assets/**"), mock("GET", "/index.html")]);
    let result = find_matching_mock(&context("GET", "/assets/js/app.js"), &mocks, None);
    assert!(result.is_some(), "wildcard route should serve a deep path");
    let result = result.unwrap();
    assert_eq!(
        result.path_params.get("*").map(String::as_str),
        Some("js/app.js")
    );
}

#[test]
fn test_numeric_constraint_wins_only_for_numeric_requests() {
    let mocks = store(vec![mock("GET", "/users/:id(\\d+)")]);
    assert!(find_matching_mock(&context("GET", "/users/42"), &mocks, None).is_some());
    assert!(find_matching_mock(&context("GET", "/users/me"), &mocks, None).is_none());
}

#[test]
fn test_existing_param_behaviour_unchanged() {
    let mocks = store(vec![mock("GET", "/users/:id")]);
    let result = find_matching_mock(&context("GET", "/users/abc"), &mocks, None)
        .expect("plain :id must still match");
    assert_eq!(
        result.path_params.get("id").map(String::as_str),
        Some("abc")
    );
}
