use super::*;
use crate::types::{FormBodyMatcher, TextBodyMatcher};

/// A bare mock with no matchers, which every request matches.
fn plain_mock(method: &str, path: &str) -> MockConfig {
    MockConfig {
        method: method.to_string(),
        path: path.to_string(),
        status: 200,
        response: serde_json::json!({"ok": true}),
        consume_body: false,
        query_params: None,
        headers: None,
        body: None,
        delay_ms: None,
        response_headers: None,
        source: None,
        sequence: None,
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

fn ctx(method: &str, path: &str) -> RequestContext {
    RequestContext::new(method.to_string(), path.to_string())
}

fn store(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for mock in mocks {
        let key = crate::types::create_mock_key(&mock.method, &mock.path);
        map.entry(key).or_default().push(mock);
    }
    map
}

fn header_matcher(name: &str, value: HeaderValue) -> HeaderMatcher {
    let mut required = HashMap::new();
    required.insert(name.to_string(), value);
    HeaderMatcher {
        required,
        forbidden: Vec::new(),
        strict: false,
    }
}

// ── explain_no_match: nothing registered ────────────────────────────

#[test]
fn explain_no_match_reports_an_unregistered_route() {
    let mocks = store(vec![plain_mock("GET", "/users")]);
    let explanation = explain_no_match(&ctx("GET", "/orders"), &mocks, None).unwrap();
    assert!(
        explanation.contains("no mock is registered for `GET:/orders`"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_points_at_the_other_method_on_the_same_path() {
    // The most common 404 by far: right path, wrong verb.
    let mocks = store(vec![
        plain_mock("POST", "/users"),
        plain_mock("PUT", "/users"),
    ]);
    let explanation = explain_no_match(&ctx("GET", "/users"), &mocks, None).unwrap();
    assert!(explanation.contains("no mock is registered for `GET:/users`"));
    assert!(explanation.contains("POST"), "{}", explanation);
    assert!(explanation.contains("PUT"), "{}", explanation);
}

// ── explain_no_match: one case per matcher kind ─────────────────────

#[test]
fn explain_no_match_names_an_absent_required_header() {
    let mut mock = plain_mock("GET", "/users");
    mock.source = Some("mocks/get_users.json".to_string());
    mock.headers = Some(header_matcher(
        "x-api-key",
        HeaderValue::Exact("secret".to_string()),
    ));

    let explanation = explain_no_match(&ctx("GET", "/users"), &store(vec![mock]), None).unwrap();
    assert!(explanation.contains("1 candidate mock(s) for `GET:/users`"));
    assert!(
        explanation.contains("mocks/get_users.json"),
        "{}",
        explanation
    );
    assert!(
        explanation.contains("required header `x-api-key` was absent"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_names_a_mismatched_header_value() {
    let mut mock = plain_mock("GET", "/users");
    mock.headers = Some(header_matcher(
        "x-tenant",
        HeaderValue::Exact("acme".to_string()),
    ));

    let mut context = ctx("GET", "/users");
    context
        .headers
        .insert("x-tenant".to_string(), "globex".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("header `x-tenant` was `globex`"),
        "{}",
        explanation
    );
    assert!(explanation.contains("expected `acme`"), "{}", explanation);
}

#[test]
fn explain_no_match_names_a_forbidden_header() {
    let mut mock = plain_mock("GET", "/users");
    mock.headers = Some(HeaderMatcher {
        required: HashMap::new(),
        forbidden: vec!["x-debug".to_string()],
        strict: false,
    });

    let mut context = ctx("GET", "/users");
    context
        .headers
        .insert("x-debug".to_string(), "1".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("forbidden header `x-debug` was present"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_names_an_absent_query_param() {
    let mut mock = plain_mock("GET", "/users");
    let mut params = HashMap::new();
    params.insert("page".to_string(), QueryParamValue::Exact("1".to_string()));
    mock.query_params = Some(QueryParamMatcher {
        params,
        strict: false,
    });

    let explanation = explain_no_match(&ctx("GET", "/users"), &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("required query param `page` was absent"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_describes_a_query_param_pattern() {
    let mut mock = plain_mock("GET", "/users");
    let mut params = HashMap::new();
    params.insert(
        "page".to_string(),
        QueryParamValue::Pattern(QueryParamPattern::Regex("^[0-9]+$".to_string())),
    );
    mock.query_params = Some(QueryParamMatcher {
        params,
        strict: false,
    });

    let mut context = ctx("GET", "/users");
    context
        .query_params
        .insert("page".to_string(), "abc".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("query param `page` was `abc`"),
        "{}",
        explanation
    );
    assert!(explanation.contains("^[0-9]+$"), "{}", explanation);
}

#[test]
fn explain_no_match_reports_a_missing_body() {
    let mut mock = plain_mock("POST", "/login");
    mock.body = Some(BodyMatcher::Json(JsonBodyMatcher {
        exact: None,
        partial: Some(serde_json::json!({"user": "alice"})),
        strict: false,
    }));

    let explanation = explain_no_match(&ctx("POST", "/login"), &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("requires a request body and none was sent"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_names_the_missing_json_field() {
    let mut mock = plain_mock("POST", "/login");
    mock.body = Some(BodyMatcher::Json(JsonBodyMatcher {
        exact: None,
        partial: Some(serde_json::json!({"user": {"name": "alice"}})),
        strict: false,
    }));

    let mut context = ctx("POST", "/login");
    context.body = Some(Bytes::from(r#"{"user":{"id":1}}"#));
    context.content_type = Some("application/json".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("missing the field `user.name`"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_names_the_mismatched_json_field() {
    let mut mock = plain_mock("POST", "/login");
    mock.body = Some(BodyMatcher::Json(JsonBodyMatcher {
        exact: None,
        partial: Some(serde_json::json!({"role": "admin"})),
        strict: false,
    }));

    let mut context = ctx("POST", "/login");
    context.body = Some(Bytes::from(r#"{"role":"guest"}"#));
    context.content_type = Some("application/json".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("field `role` has a different value"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_reports_a_text_body_substring() {
    let mut mock = plain_mock("POST", "/notes");
    mock.body = Some(BodyMatcher::Text(TextBodyMatcher {
        exact: None,
        contains: Some("urgent".to_string()),
        regex: None,
    }));

    let mut context = ctx("POST", "/notes");
    context.body = Some(Bytes::from("just a note"));
    context.content_type = Some("text/plain".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("does not contain `urgent`"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_reports_a_form_field() {
    let mut mock = plain_mock("POST", "/form");
    let mut fields = HashMap::new();
    fields.insert("plan".to_string(), "pro".to_string());
    mock.body = Some(BodyMatcher::Form(FormBodyMatcher {
        fields,
        strict: false,
    }));

    let mut context = ctx("POST", "/form");
    context.body = Some(Bytes::from("plan=free"));
    context.content_type = Some("application/x-www-form-urlencoded".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("form body field `plan` has a different value"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_reports_an_expected_empty_body() {
    let mut mock = plain_mock("POST", "/ping");
    mock.body = Some(BodyMatcher::Empty);

    let mut context = ctx("POST", "/ping");
    context.body = Some(Bytes::from("nope"));
    context.content_type = Some("text/plain".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("requires an empty body"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_covers_pattern_routes() {
    // `/users/42` never matches an exact key, so the candidate has to be
    // found through the same pattern scan the matcher itself uses.
    let mut mock = plain_mock("GET", "/users/:id");
    mock.source = Some("mocks/get_user.json".to_string());
    mock.headers = Some(header_matcher(
        "x-api-key",
        HeaderValue::Pattern(HeaderPattern::Any),
    ));

    let explanation = explain_no_match(&ctx("GET", "/users/42"), &store(vec![mock]), None).unwrap();
    assert!(
        explanation.contains("mocks/get_user.json"),
        "{}",
        explanation
    );
    assert!(
        explanation.contains("required header `x-api-key` was absent"),
        "{}",
        explanation
    );
}

#[test]
fn explain_no_match_lists_every_candidate() {
    let mut a = plain_mock("GET", "/users");
    a.source = Some("mocks/a.json".to_string());
    a.headers = Some(header_matcher(
        "x-a",
        HeaderValue::Pattern(HeaderPattern::Any),
    ));
    let mut b = plain_mock("GET", "/users");
    b.source = Some("mocks/b.json".to_string());
    b.headers = Some(header_matcher(
        "x-b",
        HeaderValue::Pattern(HeaderPattern::Any),
    ));

    let explanation = explain_no_match(&ctx("GET", "/users"), &store(vec![a, b]), None).unwrap();
    assert!(
        explanation.starts_with("2 candidate mock(s)"),
        "{}",
        explanation
    );
    assert!(explanation.contains("mocks/a.json"), "{}", explanation);
    assert!(explanation.contains("mocks/b.json"), "{}", explanation);
}

#[test]
fn explain_no_match_caps_the_candidate_list() {
    let mocks: Vec<MockConfig> = (0..8)
        .map(|i| {
            let mut mock = plain_mock("GET", "/users");
            mock.source = Some(format!("mocks/{}.json", i));
            mock.headers = Some(header_matcher(
                &format!("x-{}", i),
                HeaderValue::Pattern(HeaderPattern::Any),
            ));
            mock
        })
        .collect();

    let explanation = explain_no_match(&ctx("GET", "/users"), &store(mocks), None).unwrap();
    assert!(
        explanation.starts_with("8 candidate mock(s)"),
        "{}",
        explanation
    );
    assert!(explanation.contains("(+3 more)"), "{}", explanation);
}

#[test]
fn explain_no_match_falls_back_to_the_key_when_a_mock_has_no_source() {
    // Single-file and in-process mocks have no source file to name.
    let mut mock = plain_mock("GET", "/users");
    mock.headers = Some(header_matcher(
        "x-api-key",
        HeaderValue::Pattern(HeaderPattern::Any),
    ));

    let explanation = explain_no_match(&ctx("GET", "/users"), &store(vec![mock]), None).unwrap();
    assert!(explanation.contains("`GET:/users` #0"), "{}", explanation);
}

// ── redaction ───────────────────────────────────────────────────────

#[test]
fn explanations_never_quote_a_sensitive_header_value() {
    for name in ["authorization", "cookie", "set-cookie"] {
        let mut mock = plain_mock("GET", "/vault");
        mock.headers = Some(header_matcher(
            name,
            HeaderValue::Exact("Bearer super-secret-token".to_string()),
        ));

        let mut context = ctx("GET", "/vault");
        context
            .headers
            .insert(name.to_string(), "Bearer leaked-value".to_string());

        let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
        assert!(
            explanation.contains(name),
            "the header name is still useful: {}",
            explanation
        );
        assert!(
            !explanation.contains("super-secret-token"),
            "expected value leaked: {}",
            explanation
        );
        assert!(
            !explanation.contains("leaked-value"),
            "request value leaked: {}",
            explanation
        );
        assert!(explanation.contains("[REDACTED]"), "{}", explanation);
    }
}

#[test]
fn redaction_of_header_names_is_case_insensitive() {
    let mut mock = plain_mock("GET", "/vault");
    mock.headers = Some(header_matcher(
        "Authorization",
        HeaderValue::Exact("Bearer super-secret-token".to_string()),
    ));

    let mut context = ctx("GET", "/vault");
    context
        .headers
        .insert("authorization".to_string(), "Bearer nope".to_string());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        !explanation.contains("super-secret-token"),
        "{}",
        explanation
    );
    assert!(explanation.contains("[REDACTED]"), "{}", explanation);
}

// ── stability and bounds ────────────────────────────────────────────

#[test]
fn explanations_are_stable_across_runs() {
    // Two failing headers on one mock: whichever is reported must not
    // depend on HashMap iteration order, or screenshots and diffs of the
    // dashboard become worthless.
    let mut required = HashMap::new();
    required.insert(
        "x-zebra".to_string(),
        HeaderValue::Pattern(HeaderPattern::Any),
    );
    required.insert(
        "x-alpha".to_string(),
        HeaderValue::Pattern(HeaderPattern::Any),
    );
    let mut mock = plain_mock("GET", "/users");
    mock.headers = Some(HeaderMatcher {
        required,
        forbidden: Vec::new(),
        strict: false,
    });
    let mocks = store(vec![mock]);

    let first = explain_no_match(&ctx("GET", "/users"), &mocks, None).unwrap();
    for _ in 0..20 {
        assert_eq!(
            first,
            explain_no_match(&ctx("GET", "/users"), &mocks, None).unwrap()
        );
    }
    assert!(first.contains("x-alpha"), "{}", first);
}

#[test]
fn long_values_are_truncated_in_explanations() {
    let long = "v".repeat(500);
    let mut mock = plain_mock("GET", "/users");
    mock.headers = Some(header_matcher(
        "x-long",
        HeaderValue::Exact("expected".to_string()),
    ));

    let mut context = ctx("GET", "/users");
    context.headers.insert("x-long".to_string(), long.clone());

    let explanation = explain_no_match(&context, &store(vec![mock]), None).unwrap();
    assert!(
        !explanation.contains(&long),
        "the full value was echoed back"
    );
    assert!(explanation.contains('…'), "{}", explanation);
}

#[test]
fn truncation_lands_on_a_character_boundary() {
    // A cut mid-character would produce invalid UTF-8; `truncate_for_display`
    // counts characters, so this must round-trip.
    let value = "é".repeat(200);
    let truncated = truncate_for_display(&value);
    assert!(truncated.chars().count() <= MAX_DISPLAY_LEN + 1);
    assert!(truncated.ends_with('…'));
}

// ── the matched case ────────────────────────────────────────────────

#[test]
fn explain_match_shows_the_score_arithmetic() {
    let mut mock = plain_mock("GET", "/users");
    mock.source = Some("mocks/get_users.json".to_string());
    mock.headers = Some(header_matcher(
        "x-api-key",
        HeaderValue::Pattern(HeaderPattern::Any),
    ));

    let mut context = ctx("GET", "/users");
    context
        .headers
        .insert("x-api-key".to_string(), "abc".to_string());

    let result = find_matching_mock(&context, &store(vec![mock]), None).unwrap();
    let explanation = explain_match(&result);

    assert_eq!(result.score, 1050);
    assert!(
        explanation.contains("matched mocks/get_users.json"),
        "{}",
        explanation
    );
    assert!(explanation.contains("score 1050"), "{}", explanation);
    assert!(explanation.contains("method+path 1000"), "{}", explanation);
    assert!(explanation.contains("headers +50"), "{}", explanation);
}

#[test]
fn explain_match_notes_the_path_pattern_penalty() {
    let mock = plain_mock("GET", "/users/:id");
    let result = find_matching_mock(&ctx("GET", "/users/42"), &store(vec![mock]), None).unwrap();
    let explanation = explain_match(&result);

    assert_eq!(result.score, 900);
    assert!(explanation.contains("path pattern -100"), "{}", explanation);
    assert_eq!(result.path_params.get("id").map(String::as_str), Some("42"));
}

#[test]
fn the_breakdown_always_adds_up_to_the_score() {
    // The breakdown is what the dashboard shows; if it ever stopped summing
    // to the score the explanation would be quietly lying.
    let mut mock = plain_mock("POST", "/login");
    let mut params = HashMap::new();
    params.insert("v".to_string(), QueryParamValue::Exact("2".to_string()));
    mock.query_params = Some(QueryParamMatcher {
        params,
        strict: false,
    });
    mock.headers = Some(header_matcher(
        "x-api-key",
        HeaderValue::Pattern(HeaderPattern::Any),
    ));
    mock.body = Some(BodyMatcher::Any);

    let mut context = ctx("POST", "/login");
    context
        .query_params
        .insert("v".to_string(), "2".to_string());
    context
        .headers
        .insert("x-api-key".to_string(), "abc".to_string());
    context.body = Some(Bytes::from("{}"));
    context.content_type = Some("application/json".to_string());

    let result = find_matching_mock(&context, &store(vec![mock]), None).unwrap();
    assert_eq!(result.breakdown.total(), result.score);
    assert_eq!(result.score, 1000 + 100 + 50 + 500);
}

// ── the boolean view and the reason view agree ──────────────────────

#[test]
fn a_reason_is_produced_exactly_when_the_matcher_rejects() {
    // The guarantee the whole design rests on: `match_*` is defined as
    // `reject_reason().is_none()`, so the dashboard can never describe a
    // rule the server doesn't enforce.
    let mut required = HashMap::new();
    required.insert(
        "x-api-key".to_string(),
        HeaderValue::Exact("secret".to_string()),
    );
    let matcher = HeaderMatcher {
        required,
        forbidden: Vec::new(),
        strict: false,
    };

    let mut accepted = HashMap::new();
    accepted.insert("x-api-key".to_string(), "secret".to_string());
    assert!(match_headers(&accepted, &matcher));
    assert!(headers_reject_reason(&accepted, &matcher).is_none());

    let mut rejected = HashMap::new();
    rejected.insert("x-api-key".to_string(), "wrong".to_string());
    assert!(!match_headers(&rejected, &matcher));
    assert!(headers_reject_reason(&rejected, &matcher).is_some());
}

#[test]
fn evaluate_mock_and_calculate_match_score_agree() {
    let mut mock = plain_mock("GET", "/users");
    mock.headers = Some(header_matcher(
        "x-api-key",
        HeaderValue::Exact("secret".to_string()),
    ));

    let missing = ctx("GET", "/users");
    assert!(matches!(
        evaluate_mock(&missing, &mock),
        Evaluation::Rejected(RejectReason::HeaderMissing { .. })
    ));
    assert!(calculate_match_score(&missing, &mock).is_none());

    let mut present = ctx("GET", "/users");
    present
        .headers
        .insert("x-api-key".to_string(), "secret".to_string());
    assert!(matches!(
        evaluate_mock(&present, &mock),
        Evaluation::Matched(_)
    ));
    assert_eq!(
        calculate_match_score(&present, &mock).map(|b| b.total()),
        Some(1050)
    );
}
