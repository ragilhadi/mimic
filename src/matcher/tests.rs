use super::*;

// Query Parameter Tests
#[test]
fn test_parse_query_string() {
    let params = parse_query_string(Some("page=1&limit=10"));
    assert_eq!(params.get("page"), Some(&"1".to_string()));
    assert_eq!(params.get("limit"), Some(&"10".to_string()));
}

#[test]
fn test_parse_query_string_url_encoded() {
    let params = parse_query_string(Some("q=hello%20world"));
    assert_eq!(params.get("q"), Some(&"hello world".to_string()));
}

#[test]
fn test_parse_query_string_empty() {
    let params = parse_query_string(None);
    assert!(params.is_empty());

    let params = parse_query_string(Some(""));
    assert!(params.is_empty());
}

// ------------------------------------------------------------------
// `+` means space in form encoding (#85)
// ------------------------------------------------------------------

#[test]
fn test_parse_query_string_decodes_plus_as_a_space() {
    // Both standard encodings of the same value have to produce the same
    // value, or a mock accepts one client and rejects another for a
    // difference neither of them chose.
    for query in ["q=hello+world", "q=hello%20world"] {
        let params = parse_query_string(Some(query));
        assert_eq!(
            params.get("q"),
            Some(&"hello world".to_string()),
            "'{}' should decode to a space",
            query
        );
    }
}

#[test]
fn test_parse_query_string_decodes_plus_in_keys_too() {
    let params = parse_query_string(Some("sort+by=name"));
    assert_eq!(params.get("sort by"), Some(&"name".to_string()));
}

#[test]
fn test_parse_query_string_keeps_an_escaped_literal_plus() {
    // The decode order is what this pins down: `+` -> space first, then
    // percent-decoding. Reversed, `%2B` would become `+` and then be
    // corrupted into a space.
    let params = parse_query_string(Some("formula=1%2B1&filter=a%2Bb+c"));
    assert_eq!(params.get("formula"), Some(&"1+1".to_string()));
    assert_eq!(params.get("filter"), Some(&"a+b c".to_string()));
}

#[test]
fn test_parse_query_string_valueless_param_decodes_plus() {
    let params = parse_query_string(Some("debug+mode"));
    assert_eq!(params.get("debug mode"), Some(&String::new()));
}

#[test]
fn test_query_param_matcher_accepts_either_encoding_of_a_space() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([(
            "q".to_string(),
            QueryParamValue::Exact("hello world".to_string()),
        )]),
        strict: false,
    };

    for query in ["q=hello+world", "q=hello%20world"] {
        assert!(
            match_query_params(&parse_query_string(Some(query)), &matcher),
            "matcher written in decoded form should accept '{}'",
            query
        );
    }
}

#[test]
fn test_parse_body_form_decodes_plus_as_a_space() {
    // The reported case: `curl -d 'password=secret+pass'` against a mock
    // whose matcher reads `"password": "secret pass"`.
    for body in ["password=secret+pass", "password=secret%20pass"] {
        let parsed = parse_body(
            &Bytes::from(body),
            Some("application/x-www-form-urlencoded"),
        );
        let ParsedBody::Form(form) = parsed else {
            panic!("expected a form body for '{}'", body);
        };
        assert_eq!(form.get("password"), Some(&"secret pass".to_string()));
    }
}

#[test]
fn test_form_body_matcher_accepts_either_encoding_of_a_space() {
    let matcher = BodyMatcher::Form(FormBodyMatcher {
        fields: HashMap::from([
            ("username".to_string(), "alice".to_string()),
            ("password".to_string(), "secret pass".to_string()),
        ]),
        strict: false,
    });

    for body in [
        "username=alice&password=secret+pass",
        "username=alice&password=secret%20pass",
    ] {
        let parsed = parse_body(
            &Bytes::from(body),
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(
            body_reject_reason(&parsed, &matcher),
            None,
            "'{}' should match a matcher written in decoded form",
            body
        );
    }
}

#[test]
fn test_form_body_keeps_an_escaped_literal_plus() {
    let parsed = parse_body(
        &Bytes::from("token=a%2Bb&note=one+two"),
        Some("application/x-www-form-urlencoded"),
    );
    let ParsedBody::Form(form) = parsed else {
        panic!("expected a form body");
    };
    assert_eq!(form.get("token"), Some(&"a+b".to_string()));
    assert_eq!(form.get("note"), Some(&"one two".to_string()));
}

#[test]
fn test_match_query_params_exact() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
        strict: false,
    };

    let request = HashMap::from([("page".to_string(), "1".to_string())]);
    assert!(match_query_params(&request, &matcher));

    let request = HashMap::from([("page".to_string(), "2".to_string())]);
    assert!(!match_query_params(&request, &matcher));
}

#[test]
fn test_match_query_params_extra_ignored() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
        strict: false,
    };

    let request = HashMap::from([
        ("page".to_string(), "1".to_string()),
        ("extra".to_string(), "value".to_string()),
    ]);
    assert!(match_query_params(&request, &matcher));
}

#[test]
fn test_match_query_params_strict() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
        strict: true,
    };

    let request = HashMap::from([
        ("page".to_string(), "1".to_string()),
        ("extra".to_string(), "value".to_string()),
    ]);
    assert!(!match_query_params(&request, &matcher));
}

#[test]
fn test_match_query_params_regex() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([(
            "page".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::Regex("^[0-9]+$".to_string())),
        )]),
        strict: false,
    };

    let request = HashMap::from([("page".to_string(), "123".to_string())]);
    assert!(match_query_params(&request, &matcher));

    let request = HashMap::from([("page".to_string(), "abc".to_string())]);
    assert!(!match_query_params(&request, &matcher));
}

#[test]
fn test_match_query_params_any() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([(
            "token".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::Any),
        )]),
        strict: false,
    };

    let request = HashMap::from([("token".to_string(), "anything".to_string())]);
    assert!(match_query_params(&request, &matcher));
}

// ── Repeated query parameters (#108) ────────────────────────────────

#[test]
fn test_parse_query_string_multi_keeps_every_value_in_order() {
    let multi = parse_query_string_multi(Some("tag=a&tag=b&tag=c"));
    assert_eq!(
        multi.get("tag"),
        Some(&vec!["a".to_string(), "b".to_string(), "c".to_string()])
    );
}

/// The bug as reported: `?tag=a&tag=b` used to become `{"tag": "b"}`,
/// silently dropping `a`. The single-value view is now the *first*
/// value, documented as such, rather than whichever happened to be
/// inserted last.
#[test]
fn test_parse_query_string_keeps_the_first_value_of_a_repeated_key() {
    let single = parse_query_string(Some("tag=a&tag=b"));
    assert_eq!(single.get("tag"), Some(&"a".to_string()));
}

#[test]
fn test_a_repeated_key_still_matches_ordinary_exact_against_its_first_value() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([("tag".to_string(), QueryParamValue::Exact("a".to_string()))]),
        strict: false,
    };
    let context = {
        let mut ctx = RequestContext::new("GET".to_string(), "/search".to_string());
        ctx.query_params = parse_query_string(Some("tag=a&tag=b"));
        ctx.query_params_all = parse_query_string_multi(Some("tag=a&tag=b"));
        ctx
    };

    assert_eq!(
        query_params_reject_reason_all(
            &context.query_params,
            Some(&context.query_params_all),
            &matcher
        ),
        None
    );
}

#[test]
fn test_contains_matches_any_occurrence_of_a_repeated_key() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([(
            "tag".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::Contains("b".to_string())),
        )]),
        strict: false,
    };
    let single = parse_query_string(Some("tag=a&tag=b"));
    let multi = parse_query_string_multi(Some("tag=a&tag=b"));

    assert_eq!(
        query_params_reject_reason_all(&single, Some(&multi), &matcher),
        None,
        "'b' is among the values 'tag' carried"
    );

    let matcher_c = QueryParamMatcher {
        params: HashMap::from([(
            "tag".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::Contains("c".to_string())),
        )]),
        strict: false,
    };
    assert!(query_params_reject_reason_all(&single, Some(&multi), &matcher_c).is_some());
}

#[test]
fn test_list_matches_only_the_exact_ordered_set_of_values() {
    let single = parse_query_string(Some("tag=a&tag=b"));
    let multi = parse_query_string_multi(Some("tag=a&tag=b"));

    let matches = QueryParamMatcher {
        params: HashMap::from([(
            "tag".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::List(vec![
                "a".to_string(),
                "b".to_string(),
            ])),
        )]),
        strict: false,
    };
    assert_eq!(
        query_params_reject_reason_all(&single, Some(&multi), &matches),
        None
    );

    let wrong_order = QueryParamMatcher {
        params: HashMap::from([(
            "tag".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::List(vec![
                "b".to_string(),
                "a".to_string(),
            ])),
        )]),
        strict: false,
    };
    assert!(query_params_reject_reason_all(&single, Some(&multi), &wrong_order).is_some());

    let missing_one = QueryParamMatcher {
        params: HashMap::from([(
            "tag".to_string(),
            QueryParamValue::Pattern(QueryParamPattern::List(vec!["a".to_string()])),
        )]),
        strict: false,
    };
    assert!(query_params_reject_reason_all(&single, Some(&multi), &missing_one).is_some());
}

#[test]
fn test_strict_mode_counts_a_repeated_key_once_not_once_per_value() {
    let matcher = QueryParamMatcher {
        params: HashMap::from([("tag".to_string(), QueryParamValue::Exact("a".to_string()))]),
        strict: true,
    };
    let single = parse_query_string(Some("tag=a&tag=b&tag=c"));
    let multi = parse_query_string_multi(Some("tag=a&tag=b&tag=c"));

    assert_eq!(
        query_params_reject_reason_all(&single, Some(&multi), &matcher),
        None,
        "one declared key, however many values it carried, must not trip strict mode"
    );
}

#[test]
fn test_a_repeated_form_field_keeps_its_first_value() {
    let parsed = parse_body(
        &Bytes::from("opt=a&opt=b"),
        Some("application/x-www-form-urlencoded"),
    );
    let ParsedBody::Form(form) = parsed else {
        panic!("expected a form body");
    };
    assert_eq!(form.get("opt"), Some(&"a".to_string()));
}

// Header Tests
#[test]
fn test_match_headers_exact() {
    let matcher = HeaderMatcher {
        required: HashMap::from([(
            "authorization".to_string(),
            HeaderValue::Exact("Bearer token".to_string()),
        )]),
        forbidden: vec![],
        strict: false,
    };

    let request = HashMap::from([("authorization".to_string(), "Bearer token".to_string())]);
    assert!(match_headers(&request, &matcher));
}

#[test]
fn test_match_headers_case_insensitive() {
    let matcher = HeaderMatcher {
        required: HashMap::from([(
            "Authorization".to_string(),
            HeaderValue::Exact("Bearer token".to_string()),
        )]),
        forbidden: vec![],
        strict: false,
    };

    // Request headers are normalized to lowercase
    let request = HashMap::from([("authorization".to_string(), "Bearer token".to_string())]);
    assert!(match_headers(&request, &matcher));
}

#[test]
fn test_match_headers_prefix() {
    let matcher = HeaderMatcher {
        required: HashMap::from([(
            "authorization".to_string(),
            HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
        )]),
        forbidden: vec![],
        strict: false,
    };

    let request = HashMap::from([("authorization".to_string(), "Bearer abc123".to_string())]);
    assert!(match_headers(&request, &matcher));

    let request = HashMap::from([("authorization".to_string(), "Basic abc123".to_string())]);
    assert!(!match_headers(&request, &matcher));
}

#[test]
fn test_match_headers_forbidden() {
    let matcher = HeaderMatcher {
        required: HashMap::new(),
        forbidden: vec!["x-debug".to_string()],
        strict: false,
    };

    let request = HashMap::from([("x-debug".to_string(), "true".to_string())]);
    assert!(!match_headers(&request, &matcher));

    let request = HashMap::from([("x-other".to_string(), "value".to_string())]);
    assert!(match_headers(&request, &matcher));
}

// Body Tests
#[test]
fn test_match_json_exact() {
    let matcher = JsonBodyMatcher {
        exact: Some(serde_json::json!({"name": "Alice"})),
        partial: None,
        strict: false,
    };

    let actual = serde_json::json!({"name": "Alice"});
    assert!(match_json_body(&actual, &matcher));

    let wrong = serde_json::json!({"name": "Bob"});
    assert!(!match_json_body(&wrong, &matcher));
}

#[test]
fn test_match_json_partial() {
    let matcher = JsonBodyMatcher {
        exact: None,
        partial: Some(serde_json::json!({"name": "Alice"})),
        strict: false,
    };

    let actual = serde_json::json!({"name": "Alice", "age": 25});
    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_match_json_partial_strict() {
    let matcher = JsonBodyMatcher {
        exact: None,
        partial: Some(serde_json::json!({"name": "Alice"})),
        strict: true,
    };

    let actual = serde_json::json!({"name": "Alice", "age": 25});
    assert!(!match_json_body(&actual, &matcher));

    let actual = serde_json::json!({"name": "Alice"});
    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_match_json_nested() {
    let matcher = JsonBodyMatcher {
        exact: None,
        partial: Some(serde_json::json!({
            "user": {
                "name": "Alice"
            }
        })),
        strict: false,
    };

    let actual = serde_json::json!({
        "user": {
            "name": "Alice",
            "email": "alice@example.com"
        },
        "meta": {}
    });
    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_match_text_contains() {
    let matcher = TextBodyMatcher {
        exact: None,
        contains: Some("hello".to_string()),
        regex: None,
    };

    assert!(match_text_body("hello world", &matcher));
    assert!(!match_text_body("goodbye", &matcher));
}

#[test]
fn test_match_text_regex() {
    let matcher = TextBodyMatcher {
        exact: None,
        contains: None,
        regex: Some("^[A-Z]{3}-\\d{3}$".to_string()),
    };

    assert!(match_text_body("ABC-123", &matcher));
    assert!(!match_text_body("abc-123", &matcher));
}

#[test]
fn test_match_form_body() {
    let matcher = FormBodyMatcher {
        fields: HashMap::from([("username".to_string(), "admin".to_string())]),
        strict: false,
    };

    let actual = HashMap::from([
        ("username".to_string(), "admin".to_string()),
        ("extra".to_string(), "value".to_string()),
    ]);
    assert!(match_form_body(&actual, &matcher));
}

#[test]
fn test_match_form_body_strict() {
    let matcher = FormBodyMatcher {
        fields: HashMap::from([("username".to_string(), "admin".to_string())]),
        strict: true,
    };

    let actual = HashMap::from([
        ("username".to_string(), "admin".to_string()),
        ("extra".to_string(), "value".to_string()),
    ]);
    assert!(!match_form_body(&actual, &matcher));
}

#[test]
fn test_parse_body_json() {
    let bytes = Bytes::from(r#"{"name":"Alice"}"#);
    let parsed = parse_body(&bytes, Some("application/json"));

    if let ParsedBody::Json(json) = parsed {
        assert_eq!(json["name"], "Alice");
    } else {
        panic!("Expected JSON body");
    }
}

#[test]
fn test_parse_body_form() {
    let bytes = Bytes::from("username=admin&password=secret");
    let parsed = parse_body(&bytes, Some("application/x-www-form-urlencoded"));

    if let ParsedBody::Form(form) = parsed {
        assert_eq!(form.get("username"), Some(&"admin".to_string()));
        assert_eq!(form.get("password"), Some(&"secret".to_string()));
    } else {
        panic!("Expected Form body");
    }
}

#[test]
fn test_parse_body_empty() {
    let bytes = Bytes::new();
    let parsed = parse_body(&bytes, Some("application/json"));

    assert!(matches!(parsed, ParsedBody::Empty));
}

#[test]
fn test_find_matching_mock_returns_index() {
    let make_mock = |role: &str| MockConfig {
        method: "POST".to_string(),
        path: "/login".to_string(),
        status: 200,
        response: serde_json::json!({"role": role}),
        consume_body: true,
        query_params: None,
        headers: None,
        body: Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: None,
            partial: Some(serde_json::json!({"role": role})),
            strict: false,
        })),
        delay_ms: None,
        response_headers: None,
        source: None,
        sequence: None,
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    };

    let mocks = HashMap::from([(
        "POST:/login".to_string(),
        vec![make_mock("admin"), make_mock("user")],
    )]);

    let make_context = |role: &str| RequestContext {
        method: "POST".to_string(),
        path: "/login".to_string(),
        path_params: HashMap::new(),
        query_params: HashMap::new(),
        query_params_all: HashMap::new(),
        headers: HashMap::new(),
        body: Some(Bytes::from(format!(r#"{{"role":"{}"}}"#, role))),
        content_type: Some("application/json".to_string()),
    };

    let result = find_matching_mock(&make_context("admin"), &mocks, None).unwrap();
    assert_eq!(result.index, 0);
    assert_eq!(result.mock.response["role"], "admin");
    assert!(result.path_params.is_empty());
    assert_eq!(result.matched_key, "POST:/login");

    let result = find_matching_mock(&make_context("user"), &mocks, None).unwrap();
    assert_eq!(result.index, 1);
    assert_eq!(result.mock.response["role"], "user");
}

// ========================================================================
// Path Parameter Tests
// ========================================================================

#[test]
fn test_is_pattern_path() {
    assert!(is_pattern_path("/users/:id"));
    assert!(is_pattern_path("/orgs/{org}/repos/{repo}"));
    assert!(is_pattern_path("/users/:id/posts/:postId"));
    assert!(!is_pattern_path("/users/123"));
    assert!(!is_pattern_path("/users"));
    assert!(!is_pattern_path("/"));
}

#[test]
fn test_compile_path_pattern_single_param_colon_style() {
    let pattern = compile_path_pattern("/users/:id").unwrap();
    assert_eq!(pattern.param_names, vec!["id".to_string()]);
    assert!(pattern.regex.is_match("/users/42"));
    assert!(!pattern.regex.is_match("/users/42/extra"));
    assert!(!pattern.regex.is_match("/users"));
}

#[test]
fn test_compile_path_pattern_single_param_brace_style() {
    let pattern = compile_path_pattern("/users/{id}").unwrap();
    assert_eq!(pattern.param_names, vec!["id".to_string()]);
    assert!(pattern.regex.is_match("/users/42"));
}

#[test]
fn test_compile_path_pattern_multiple_params() {
    let pattern = compile_path_pattern("/orgs/{org}/repos/{repo}").unwrap();
    assert_eq!(
        pattern.param_names,
        vec!["org".to_string(), "repo".to_string()]
    );
    assert!(pattern.regex.is_match("/orgs/acme/repos/widgets"));
}

#[test]
fn test_compile_path_pattern_nested_mixed_literal_segments() {
    let pattern = compile_path_pattern("/orders/:id/items/:itemId").unwrap();
    assert_eq!(
        pattern.param_names,
        vec!["id".to_string(), "itemId".to_string()]
    );
    assert!(pattern.regex.is_match("/orders/ORD-9981/items/2"));
    assert!(!pattern.regex.is_match("/orders/ORD-9981"));
}

#[test]
fn test_compile_path_pattern_returns_none_without_params() {
    assert!(compile_path_pattern("/users/123").is_none());
    assert!(compile_path_pattern("/users").is_none());
}

#[test]
fn test_compile_path_pattern_escapes_regex_metacharacters_in_literals() {
    // A literal segment that looks like a regex special char shouldn't
    // be interpreted as one.
    let pattern = compile_path_pattern("/a.b/:id").unwrap();
    assert!(pattern.regex.is_match("/a.b/1"));
    assert!(!pattern.regex.is_match("/aXb/1"));
}

#[test]
fn test_match_path_pattern_captures_values() {
    let pattern = compile_path_pattern("/orgs/{org}/repos/{repo}").unwrap();
    let params = match_path_pattern(&pattern, "/orgs/acme/repos/widgets").unwrap();
    assert_eq!(params.get("org"), Some(&"acme".to_string()));
    assert_eq!(params.get("repo"), Some(&"widgets".to_string()));
}

#[test]
fn test_match_path_pattern_no_match_returns_none() {
    let pattern = compile_path_pattern("/users/:id").unwrap();
    assert!(match_path_pattern(&pattern, "/posts/1").is_none());
    assert!(match_path_pattern(&pattern, "/users/1/extra").is_none());
}

fn path_param_mock(path: &str, response: serde_json::Value) -> MockConfig {
    MockConfig {
        method: "GET".to_string(),
        path: path.to_string(),
        status: 200,
        response,
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

fn path_param_context(path: &str) -> RequestContext {
    RequestContext {
        method: "GET".to_string(),
        path: path.to_string(),
        path_params: HashMap::new(),
        query_params: HashMap::new(),
        query_params_all: HashMap::new(),
        headers: HashMap::new(),
        body: None,
        content_type: None,
    }
}

#[test]
fn test_find_matching_mock_single_path_param() {
    let mocks = HashMap::from([(
        "GET:/users/:id".to_string(),
        vec![path_param_mock(
            "/users/:id",
            serde_json::json!({"name": "Mock User"}),
        )],
    )]);

    let result = find_matching_mock(&path_param_context("/users/42"), &mocks, None).unwrap();
    assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
    assert_eq!(result.matched_key, "GET:/users/:id");
    // Below the 1000 base score of an exact match
    assert!(result.score < 1000);
}

#[test]
fn test_find_matching_mock_multiple_path_params() {
    let mocks = HashMap::from([(
        "DELETE:/orgs/{org}/repos/{repo}".to_string(),
        vec![MockConfig {
            method: "DELETE".to_string(),
            ..path_param_mock("/orgs/{org}/repos/{repo}", serde_json::json!(null))
        }],
    )]);

    let context = RequestContext {
        method: "DELETE".to_string(),
        path: "/orgs/acme/repos/widgets".to_string(),
        path_params: HashMap::new(),
        query_params: HashMap::new(),
        query_params_all: HashMap::new(),
        headers: HashMap::new(),
        body: None,
        content_type: None,
    };

    let result = find_matching_mock(&context, &mocks, None).unwrap();
    assert_eq!(result.path_params.get("org"), Some(&"acme".to_string()));
    assert_eq!(result.path_params.get("repo"), Some(&"widgets".to_string()));
}

#[test]
fn test_find_matching_mock_nested_path_params() {
    let mocks = HashMap::from([(
        "GET:/orders/:id/items/:itemId".to_string(),
        vec![path_param_mock(
            "/orders/:id/items/:itemId",
            serde_json::json!({"ok": true}),
        )],
    )]);

    let result = find_matching_mock(
        &path_param_context("/orders/ORD-9981/items/2"),
        &mocks,
        None,
    )
    .unwrap();
    assert_eq!(result.path_params.get("id"), Some(&"ORD-9981".to_string()));
    assert_eq!(result.path_params.get("itemId"), Some(&"2".to_string()));
}

#[test]
fn test_find_matching_mock_exact_path_beats_pattern() {
    let mocks = HashMap::from([
        (
            "GET:/users/:id".to_string(),
            vec![path_param_mock(
                "/users/:id",
                serde_json::json!({"source": "pattern"}),
            )],
        ),
        (
            "GET:/users/42".to_string(),
            vec![path_param_mock(
                "/users/42",
                serde_json::json!({"source": "exact"}),
            )],
        ),
    ]);

    let result = find_matching_mock(&path_param_context("/users/42"), &mocks, None).unwrap();
    assert_eq!(result.mock.response["source"], "exact");
    assert!(result.path_params.is_empty());

    // A different id has no exact mock, so the pattern still matches
    let result = find_matching_mock(&path_param_context("/users/7"), &mocks, None).unwrap();
    assert_eq!(result.mock.response["source"], "pattern");
    assert_eq!(result.path_params.get("id"), Some(&"7".to_string()));
}

#[test]
fn test_find_matching_mock_pattern_no_match_returns_none() {
    let mocks = HashMap::from([(
        "GET:/users/:id".to_string(),
        vec![path_param_mock(
            "/users/:id",
            serde_json::json!({"name": "Mock User"}),
        )],
    )]);

    assert!(find_matching_mock(&path_param_context("/posts/1"), &mocks, None).is_none());
}

// ------------------------------------------------------------------
// Trailing-slash tolerance (#119)
// ------------------------------------------------------------------

#[test]
fn test_trailing_slash_variant_toggles_both_directions() {
    assert_eq!(
        trailing_slash_variant("/things"),
        Some("/things/".to_string())
    );
    assert_eq!(
        trailing_slash_variant("/things/"),
        Some("/things".to_string())
    );
}

#[test]
fn test_trailing_slash_variant_leaves_the_root_path_alone() {
    assert_eq!(trailing_slash_variant("/"), None);
}

#[test]
fn test_a_mock_registered_without_a_slash_serves_a_request_with_one() {
    let mocks = mock_map(vec![plain_mock("GET", "/things", "things")]);

    let result = find_matching_mock(&path_param_context("/things/"), &mocks, None).unwrap();

    assert_eq!(result.mock.response["source"], "things");
    assert_eq!(result.matched_key, "GET:/things");
    assert!(result.trailing_slash_normalized);
}

#[test]
fn test_a_mock_registered_with_a_slash_serves_a_request_without_one() {
    let mocks = mock_map(vec![plain_mock("GET", "/things/", "things")]);

    let result = find_matching_mock(&path_param_context("/things"), &mocks, None).unwrap();

    assert_eq!(result.mock.response["source"], "things");
    assert_eq!(result.matched_key, "GET:/things/");
    assert!(result.trailing_slash_normalized);
}

#[test]
fn test_an_exact_match_never_falls_back_to_the_trailing_slash_variant() {
    // Deliberate `/things` and `/things/` mocks serve independently —
    // the fallback only ever runs when the literal path matched nothing
    // at all.
    let mocks = mock_map(vec![
        plain_mock("GET", "/things", "no-slash"),
        plain_mock("GET", "/things/", "with-slash"),
    ]);

    let no_slash = find_matching_mock(&path_param_context("/things"), &mocks, None).unwrap();
    assert_eq!(no_slash.mock.response["source"], "no-slash");
    assert!(!no_slash.trailing_slash_normalized);

    let with_slash = find_matching_mock(&path_param_context("/things/"), &mocks, None).unwrap();
    assert_eq!(with_slash.mock.response["source"], "with-slash");
    assert!(!with_slash.trailing_slash_normalized);
}

#[test]
fn test_a_pattern_route_matches_a_request_with_a_trailing_slash() {
    let mocks = HashMap::from([(
        "GET:/users/:id".to_string(),
        vec![path_param_mock(
            "/users/:id",
            serde_json::json!({"name": "Mock User"}),
        )],
    )]);

    let result = find_matching_mock(&path_param_context("/users/42/"), &mocks, None).unwrap();
    assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
    assert!(result.trailing_slash_normalized);
}

#[test]
fn test_the_root_path_never_falls_back_to_anything() {
    let mocks = mock_map(vec![plain_mock("GET", "/health-ish", "unrelated")]);
    assert!(find_matching_mock(&path_param_context("/"), &mocks, None).is_none());
}

#[test]
fn test_route_exists_true_for_a_trailing_slash_variant_of_a_registered_path() {
    let mocks = mock_map(vec![plain_mock("GET", "/things", "things")]);
    assert!(route_exists("GET", "/things/", &mocks, None));
    assert!(!route_exists("POST", "/things/", &mocks, None));
}

#[test]
fn test_requires_body_true_when_only_a_trailing_slash_variant_mock_needs_it() {
    let mock = MockConfig {
        consume_body: true,
        ..plain_mock("POST", "/submit", "submit")
    };
    let mocks = mock_map(vec![mock]);
    assert!(requires_body("POST", "/submit/", &mocks, None));
}

#[test]
fn test_explain_match_notes_trailing_slash_normalization() {
    let mocks = mock_map(vec![plain_mock("GET", "/things", "things")]);
    let result = find_matching_mock(&path_param_context("/things/"), &mocks, None).unwrap();
    assert!(explain_match(&result).contains("trailing slash normalized"));
}

#[test]
fn test_explain_no_match_finds_a_body_matcher_rejection_via_trailing_slash() {
    // A mock registered at `/things` (no slash) declares a body matcher
    // that this request's body doesn't satisfy. The 404 explanation must
    // name that rejection rather than claim nothing is registered at all
    // — the whole point of aligning the fallback with `find_matching_mock`.
    let mock = MockConfig {
        body: Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: Some(serde_json::json!({"ok": true})),
            partial: None,
            strict: false,
        })),
        ..plain_mock("POST", "/things", "things")
    };
    let mocks = mock_map(vec![mock]);

    let mut context = path_param_context("/things/");
    context.method = "POST".to_string();
    context.body = Some(Bytes::from(r#"{"ok": false}"#));
    context.content_type = Some("application/json".to_string());

    let explanation = explain_no_match(&context, &mocks, None).unwrap();
    assert!(explanation.contains("candidate mock"), "{}", explanation);
    assert!(
        explanation.contains("trailing-slash normalization"),
        "{}",
        explanation
    );
}

#[test]
fn test_explain_no_match_reports_a_still_unregistered_path_even_after_the_trailing_slash_check() {
    let mocks = mock_map(vec![plain_mock("GET", "/unrelated", "unrelated")]);
    let explanation = explain_no_match(&path_param_context("/things/"), &mocks, None).unwrap();
    assert!(
        explanation.contains("no mock is registered"),
        "{}",
        explanation
    );
    assert!(
        explanation.contains("trailing-slash variant was also checked"),
        "{}",
        explanation
    );
}

#[test]
fn test_strict_trailing_slash_env_var_disables_the_fallback() {
    // Serialized by [`env_guard`]: this test flips MIMIC_STRICT_TRAILING_SLASH,
    // and every other test relying on the tolerant (default) behavior
    // holds the same lock. See #112, which added a second knob of the
    // same kind and needed the coordination to be explicit.
    let _env = env_guard();
    let restore = std::env::var(STRICT_TRAILING_SLASH_ENV).ok();
    let mocks = mock_map(vec![plain_mock("GET", "/things", "things")]);

    std::env::set_var(STRICT_TRAILING_SLASH_ENV, "true");
    assert!(
        find_matching_mock(&path_param_context("/things/"), &mocks, None).is_none(),
        "strict mode must not fall back to the trailing-slash variant"
    );
    assert!(!route_exists("GET", "/things/", &mocks, None));
    assert!(
        requires_body("GET", "/things/", &mocks, None),
        "no candidate at all means the body is worth logging"
    );

    std::env::remove_var(STRICT_TRAILING_SLASH_ENV);
    assert!(
        find_matching_mock(&path_param_context("/things/"), &mocks, None).is_some(),
        "unset must restore the tolerant default"
    );

    match restore {
        Some(value) => std::env::set_var(STRICT_TRAILING_SLASH_ENV, value),
        None => std::env::remove_var(STRICT_TRAILING_SLASH_ENV),
    }
}

// ------------------------------------------------------------------
// HEAD answered from the GET mock (#112)
// ------------------------------------------------------------------

/// Serialized access to the process environment: held by every test in
/// this module that mutates an env var, and by every test whose behavior
/// depends on one's default — through the same lock the async handler
/// tests await, so the two groups serialize against each other too. See
/// [`crate::types::env_lock`].
fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    crate::types::env_lock().blocking_lock()
}

fn head_context(path: &str) -> RequestContext {
    RequestContext {
        method: "HEAD".to_string(),
        ..path_param_context(path)
    }
}

#[test]
fn test_head_request_falls_back_to_the_get_mock() {
    let _env = env_guard();
    let mocks = mock_map(vec![plain_mock("GET", "/users", "users")]);

    let result = find_matching_mock(&head_context("/users"), &mocks, None)
        .expect("HEAD with only a GET mock registered should fall back to it");
    assert!(
        result.head_from_get,
        "the win must be marked as the fallback"
    );
    assert_eq!(result.matched_key, "GET:/users");
    // Today's lookup (the flag off, i.e. what a pre-#112 Mimic did) is
    // what any *other* method continues to do unchanged.
    assert!(
        find_matching_mock(&path_param_context("/users"), &mocks, None).is_some(),
        "GET keeps matching its own mock"
    );
    assert!(
        find_matching_mock(
            &RequestContext {
                method: "PUT".to_string(),
                ..path_param_context("/users")
            },
            &mocks,
            None
        )
        .is_none(),
        "PUT gets no such rewrite"
    );
}

#[test]
fn test_an_explicit_head_mock_wins_over_the_get_fallback() {
    let _env = env_guard();
    let mocks = mock_map(vec![
        plain_mock("GET", "/users", "get-users"),
        plain_mock("HEAD", "/users", "head-users"),
    ]);

    let result = find_matching_mock(&head_context("/users"), &mocks, None)
        .expect("the explicit HEAD mock must match");
    assert_eq!(
        result.mock.response,
        serde_json::json!({"source": "head-users"})
    );
    assert!(
        !result.head_from_get,
        "an explicit HEAD mock is not the fallback"
    );
}

#[test]
fn test_head_fallback_matches_a_pattern_route() {
    let _env = env_guard();
    let mocks = mock_map(vec![path_param_mock(
        "/users/:id",
        serde_json::json!({"name": "Mock User"}),
    )]);

    let result = find_matching_mock(&head_context("/users/42"), &mocks, None)
        .expect("HEAD /users/42 should be answered by GET /users/:id");
    assert!(result.head_from_get);
    assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
}

#[test]
fn test_head_fallback_composes_with_trailing_slash_normalization() {
    let _env = env_guard();
    let mocks = mock_map(vec![plain_mock("GET", "/things", "things")]);

    let result = find_matching_mock(&head_context("/things/"), &mocks, None).expect("should match");
    assert!(result.head_from_get, "answered via the GET rewrite");
    assert!(
        result.trailing_slash_normalized,
        "and via the slash variant"
    );
}

#[test]
fn test_head_fallback_evaluates_the_get_mock_matchers_against_the_request() {
    let _env = env_guard();
    let mut query = plain_mock("GET", "/users", "users");
    query.query_params = Some(QueryParamMatcher {
        params: HashMap::from([("page".to_string(), QueryParamValue::Exact("1".to_string()))]),
        strict: false,
    });
    let mocks = mock_map(vec![query.clone()]);

    let mut request = head_context("/users");
    request.query_params = parse_query_string(Some("page=1"));
    assert!(
        find_matching_mock(&request, &mocks, None).is_some(),
        "a HEAD carrying page=1 satisfies the GET mock's query matcher"
    );

    request.query_params.clear();
    assert!(
        find_matching_mock(&request, &mocks, None).is_none(),
        "a HEAD failing the GET mock's matcher is still a miss"
    );
}

#[test]
fn test_route_exists_for_head_when_only_the_get_mock_is_registered() {
    let _env = env_guard();
    let mocks = mock_map(vec![plain_mock("GET", "/users", "users")]);

    assert!(
        route_exists("HEAD", "/users", &mocks, None),
        "a HEAD preflight sees the endpoint GET registered"
    );
    assert!(
        !route_exists("HEAD", "/nope", &mocks, None),
        "a HEAD preflight for an unregistered path is still a miss"
    );
}

#[test]
fn test_explain_match_names_the_get_mock_for_the_head_fallback() {
    let _env = env_guard();
    let mut source_mock = plain_mock("GET", "/things", "things");
    source_mock.source = Some("mocks/things.json".to_string());
    let mocks = mock_map(vec![source_mock]);

    let result = find_matching_mock(&head_context("/things"), &mocks, None).unwrap();
    let explanation = explain_match(&result);
    assert!(
        explanation.contains("HEAD answered from the GET mock at `GET:/things`"),
        "{}",
        explanation
    );
    assert!(explanation.contains("mocks/things.json"), "{}", explanation);
}

#[test]
fn test_head_from_get_env_var_disables_the_fallback() {
    // Serialized by [`env_guard`]: this test flips MIMIC_HEAD_FROM_GET,
    // and every other test relying on the enabled (default) behavior
    // holds the same lock.
    let _env = env_guard();
    let restore = std::env::var(HEAD_FROM_GET_ENV).ok();
    let mocks = mock_map(vec![plain_mock("GET", "/users", "users")]);

    std::env::set_var(HEAD_FROM_GET_ENV, "false");
    assert!(
        find_matching_mock(&head_context("/users"), &mocks, None).is_none(),
        "false must restore the pre-#112 404"
    );
    assert!(
        !route_exists("HEAD", "/users", &mocks, None),
        "a preflight for HEAD must keep the OFF behavior too"
    );

    std::env::set_var(HEAD_FROM_GET_ENV, "true");
    assert!(
        find_matching_mock(&head_context("/users"), &mocks, None).is_some(),
        "an explicit true keeps the fallback"
    );

    std::env::remove_var(HEAD_FROM_GET_ENV);
    assert!(
        find_matching_mock(&head_context("/users"), &mocks, None).is_some(),
        "unset must restore the enabled default"
    );

    match restore {
        Some(value) => std::env::set_var(HEAD_FROM_GET_ENV, value),
        None => std::env::remove_var(HEAD_FROM_GET_ENV),
    }
}

// ------------------------------------------------------------------
// Compilation caching / pattern short-circuit
// ------------------------------------------------------------------

fn plain_mock(method: &str, path: &str, marker: &str) -> MockConfig {
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
        sequence: None,
        tags: Vec::new(),
        response_file: None,
        template: None,
        response_bytes: None,
        template_escape: None,
    }
}

fn mock_map(mocks: Vec<MockConfig>) -> HashMap<String, Vec<MockConfig>> {
    let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for mock in mocks {
        map.entry(crate::types::create_mock_key(&mock.method, &mock.path))
            .or_default()
            .push(mock);
    }
    map
}

#[test]
fn test_path_pattern_compiled_once_across_many_requests() {
    // A template unique to this test, so the shared process-wide cache
    // can't have been populated by another test.
    let template = "/cache-probe-once/:id";
    assert!(!path_pattern_is_cached(template));

    let mocks = mock_map(vec![plain_mock("GET", template, "pattern")]);

    for i in 0..50 {
        let mut ctx = RequestContext::new("GET".to_string(), format!("/cache-probe-once/{}", i));
        ctx.path_params = HashMap::new();
        let result = find_matching_mock(&ctx, &mocks, None).expect("pattern should match");
        assert_eq!(result.mock.response["source"], "pattern");
    }

    // Compilation is memoized by template string, so the 50 requests above
    // share the single entry created by the first one.
    assert!(path_pattern_is_cached(template));
    let first = compile_path_pattern(template).unwrap();
    let second = compile_path_pattern(template).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn test_exact_match_never_compiles_pattern_routes() {
    let template = "/cache-probe-skip/:id";
    assert!(!path_pattern_is_cached(template));

    let mocks = mock_map(vec![
        plain_mock("GET", "/cache-probe-skip/42", "exact"),
        plain_mock("GET", template, "pattern"),
    ]);

    let ctx = RequestContext::new("GET".to_string(), "/cache-probe-skip/42".to_string());
    let result = find_matching_mock(&ctx, &mocks, None).expect("exact should match");

    assert_eq!(result.mock.response["source"], "exact");
    assert!(result.path_params.is_empty());
    // The pattern loop never ran, so its template was never compiled.
    assert!(
        !path_pattern_is_cached(template),
        "exact-match request reached the path-pattern compiler"
    );
}

#[test]
fn test_pattern_still_used_when_exact_key_exists_but_does_not_match() {
    let template = "/cache-probe-fallthrough/:id";
    let mut exact = plain_mock("GET", "/cache-probe-fallthrough/42", "exact");
    exact.query_params = Some(QueryParamMatcher {
        params: HashMap::from([(
            "required".to_string(),
            QueryParamValue::Exact("yes".to_string()),
        )]),
        strict: false,
    });

    let mocks = mock_map(vec![exact, plain_mock("GET", template, "pattern")]);

    // The exact key exists but its query matcher rejects the request, so
    // the pattern scan must still run.
    let ctx = RequestContext::new("GET".to_string(), "/cache-probe-fallthrough/42".to_string());
    let result = find_matching_mock(&ctx, &mocks, None).expect("pattern should match");

    assert_eq!(result.mock.response["source"], "pattern");
    assert_eq!(result.path_params.get("id"), Some(&"42".to_string()));
}

#[test]
fn test_matcher_regex_is_compiled_once_per_pattern() {
    let pattern = "^probe-[a-z]+$";
    let matcher = HeaderMatcher {
        required: HashMap::from([(
            "x-probe".to_string(),
            HeaderValue::Pattern(HeaderPattern::Regex(pattern.to_string())),
        )]),
        forbidden: Vec::new(),
        strict: false,
    };

    let mut headers = HashMap::new();
    headers.insert("x-probe".to_string(), "probe-value".to_string());

    for _ in 0..100 {
        assert!(match_headers(&headers, &matcher));
    }

    // Every one of those calls resolved to the same compiled handle.
    let a = crate::regex_cache::compiled_regex(pattern).unwrap();
    let b = crate::regex_cache::compiled_regex(pattern).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn test_invalid_matcher_regex_never_matches() {
    let matcher = HeaderMatcher {
        required: HashMap::from([(
            "x-bad".to_string(),
            HeaderValue::Pattern(HeaderPattern::Regex("([unclosed".to_string())),
        )]),
        forbidden: Vec::new(),
        strict: false,
    };

    let mut headers = HashMap::new();
    headers.insert("x-bad".to_string(), "anything".to_string());

    assert!(!match_headers(&headers, &matcher));
}
// ------------------------------------------------------------------
// Deterministic tie-breaking (#55)
// ------------------------------------------------------------------

#[test]
fn test_equal_score_patterns_resolve_identically_regardless_of_insertion_order() {
    let specific = plain_mock("GET", "/users/:id", "specific");
    let generic = plain_mock("GET", "/{resource}/:id", "generic");

    // Same two mocks, opposite insertion order. HashMap iteration order is
    // reseeded per map, so this is exactly the shape that used to flip.
    let forward = mock_map(vec![specific.clone(), generic.clone()]);
    let reverse = mock_map(vec![generic, specific]);

    let ctx = RequestContext::new("GET".to_string(), "/users/42".to_string());

    let a = find_matching_mock(&ctx, &forward, None).expect("should match");
    let b = find_matching_mock(&ctx, &reverse, None).expect("should match");

    assert_eq!(a.matched_key, b.matched_key);
    assert_eq!(a.mock.response["source"], b.mock.response["source"]);
    // More literal segments wins: /users/:id over /{resource}/:id.
    assert_eq!(a.mock.response["source"], "specific");
}

#[test]
fn test_equal_specificity_tie_breaks_on_key_lexicographically() {
    // Both have zero literal segments, so the key decides.
    let alpha = plain_mock("GET", "/{alpha}/:sub", "alpha");
    let beta = plain_mock("GET", "/{beta}/:sub", "beta");

    let forward = mock_map(vec![alpha.clone(), beta.clone()]);
    let reverse = mock_map(vec![beta, alpha]);

    let ctx = RequestContext::new("GET".to_string(), "/anything/42".to_string());

    let a = find_matching_mock(&ctx, &forward, None).expect("should match");
    let b = find_matching_mock(&ctx, &reverse, None).expect("should match");

    assert_eq!(a.matched_key, b.matched_key);
    assert_eq!(a.mock.response["source"], "alpha");
}

#[test]
fn test_repeated_lookups_are_stable_across_freshly_built_maps() {
    let specific = plain_mock("GET", "/orders/:id", "specific");
    let generic = plain_mock("GET", "/{resource}/:id", "generic");
    let ctx = RequestContext::new("GET".to_string(), "/orders/7".to_string());

    // Rebuilding the map is what a hot reload does; each rebuild gets a
    // fresh hasher seed.
    let winner = {
        let map = mock_map(vec![specific.clone(), generic.clone()]);
        find_matching_mock(&ctx, &map, None).unwrap().matched_key
    };

    for _ in 0..25 {
        let map = mock_map(vec![specific.clone(), generic.clone()]);
        assert_eq!(
            find_matching_mock(&ctx, &map, None).unwrap().matched_key,
            winner
        );
    }
}

#[test]
fn test_path_specificity_counts_literal_segments_only() {
    assert_eq!(path_specificity("/users/42"), 2);
    assert_eq!(path_specificity("/users/:id"), 1);
    assert_eq!(path_specificity("/{resource}/:id"), 0);
    assert_eq!(path_specificity("/orgs/{org}/repos/{repo}"), 2);
}

// ------------------------------------------------------------------
// Strict header mode ignores client-default headers (#56)
// ------------------------------------------------------------------

fn strict_bearer_matcher() -> HeaderMatcher {
    HeaderMatcher {
        required: HashMap::from([(
            "authorization".to_string(),
            HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
        )]),
        forbidden: Vec::new(),
        strict: true,
    }
}

#[test]
fn test_strict_mode_allows_implicit_accept_header() {
    // What `curl -H "Authorization: Bearer abc" ...` actually sends.
    let headers = HashMap::from([
        ("host".to_string(), "localhost:8080".to_string()),
        ("user-agent".to_string(), "curl/8.5.0".to_string()),
        ("accept".to_string(), "*/*".to_string()),
        ("authorization".to_string(), "Bearer abc123".to_string()),
    ]);

    assert!(match_headers(&headers, &strict_bearer_matcher()));
}

#[test]
fn test_strict_mode_ignores_every_documented_default_header() {
    let mut headers = HashMap::from([("authorization".to_string(), "Bearer abc".to_string())]);
    for ignored in IGNORED_HEADERS {
        headers.insert(ignored.to_string(), "value".to_string());
    }

    assert!(match_headers(&headers, &strict_bearer_matcher()));
}

#[test]
fn test_strict_mode_still_rejects_genuinely_extra_headers() {
    let headers = HashMap::from([
        ("accept".to_string(), "*/*".to_string()),
        ("authorization".to_string(), "Bearer abc123".to_string()),
        ("x-unexpected".to_string(), "surprise".to_string()),
    ]);

    assert!(!match_headers(&headers, &strict_bearer_matcher()));
}

/// The #107 repro: a browser's `fetch()` carries a family of ambient
/// headers beyond what curl sends, and none of them should make a strict
/// mock reject a request that would otherwise match.
#[test]
fn test_strict_mode_allows_a_browsers_ambient_headers() {
    let headers = HashMap::from([
        ("authorization".to_string(), "Bearer abc".to_string()),
        ("origin".to_string(), "http://localhost:3000".to_string()),
        ("referer".to_string(), "http://localhost:3000/".to_string()),
        ("accept-language".to_string(), "en-US".to_string()),
        ("cache-control".to_string(), "no-cache".to_string()),
        ("pragma".to_string(), "no-cache".to_string()),
        ("dnt".to_string(), "1".to_string()),
        ("upgrade-insecure-requests".to_string(), "1".to_string()),
        ("sec-fetch-mode".to_string(), "cors".to_string()),
        ("sec-fetch-site".to_string(), "same-origin".to_string()),
        ("sec-fetch-dest".to_string(), "empty".to_string()),
        (
            "sec-ch-ua".to_string(),
            "\"Chromium\";v=\"120\"".to_string(),
        ),
        ("sec-ch-ua-platform".to_string(), "\"Linux\"".to_string()),
    ]);

    assert!(
        match_headers(&headers, &strict_bearer_matcher()),
        "none of a browser's ambient headers should trip strict mode"
    );
}

/// A future `sec-*` header Mimic has never heard of must still be
/// ignored — the whole point of matching by prefix instead of an
/// enumerated list.
#[test]
fn test_strict_mode_ignores_any_sec_prefixed_header() {
    let headers = HashMap::from([
        ("authorization".to_string(), "Bearer abc".to_string()),
        ("sec-fetch-storage-access".to_string(), "active".to_string()),
        (
            "sec-ch-ua-form-factors".to_string(),
            "\"Desktop\"".to_string(),
        ),
    ]);

    assert!(match_headers(&headers, &strict_bearer_matcher()));
}

/// A genuinely undeclared, meaningful header must still be rejected —
/// widening the ignore list must not make strict mode toothless.
#[test]
fn test_strict_mode_still_rejects_a_meaningful_undeclared_header() {
    let headers = HashMap::from([
        ("authorization".to_string(), "Bearer abc".to_string()),
        ("x-tenant".to_string(), "acme".to_string()),
    ]);

    let reason = headers_reject_reason(&headers, &strict_bearer_matcher());
    match reason {
        Some(RejectReason::HeaderExtra { name }) => assert_eq!(name, "x-tenant"),
        other => panic!("expected a HeaderExtra rejection, got {:?}", other),
    }
}
