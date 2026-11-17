use super::*;

// =====================================================================
// #143 — JSON body matcher forms: $any, $regex, $type, $contains
// =====================================================================

fn expected(partial: serde_json::Value) -> JsonBodyMatcher {
    JsonBodyMatcher {
        partial: Some(partial),
        ..Default::default()
    }
}

#[test]
fn test_dollar_any_accepts_any_value() {
    let actual = serde_json::json!({"type": "payment", "id": "7f1c-generated"});
    let matcher = expected(serde_json::json!({"type": "payment", "id": {"$any": true}}));

    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_dollar_any_accepts_null_and_missing_inside_nested_object() {
    // `$any` accepts a present value of any shape, including null.
    let actual = serde_json::json!({"a": {"x": null}});
    let matcher = expected(serde_json::json!({"a": {"x": {"$any": true}}}));
    assert!(match_json_body(&actual, &matcher));

    // But a missing field is still a missing field — `$any` does not make
    // the field optional, it makes its *value* accepted.
    let missing = serde_json::json!({"a": {}});
    let reason = json_body_reject_reason(&missing, &matcher);
    match reason {
        Some(RejectReason::JsonFieldMissing { field }) => assert_eq!(field, "a.x"),
        other => panic!("expected JsonFieldMissing, got {:?}", other),
    }
}

#[test]
fn test_dollar_regex_matches_string_field() {
    let actual = serde_json::json!({"email": "alice@example.com"});
    let matcher = expected(serde_json::json!({
        "email": {"$regex": "^[^@]+@example\\.com$"}
    }));

    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_dollar_regex_rejects_non_matching_value_with_precise_reason() {
    let actual = serde_json::json!({"email": "alice@other.org"});
    let matcher = expected(serde_json::json!({
        "email": {"$regex": "^[^@]+@example\\.com$"}
    }));

    let reason = json_body_reject_reason(&actual, &matcher);
    match reason {
        Some(RejectReason::JsonRegexMismatch { field, pattern }) => {
            assert_eq!(field, "email");
            assert!(pattern.contains("@example"));
        }
        other => panic!("expected JsonRegexMismatch, got {:?}", other),
    }
}

#[test]
fn test_dollar_regex_against_non_string_actual_rejects() {
    let actual = serde_json::json!({"code": 42});
    let matcher = expected(serde_json::json!({"code": {"$regex": "^\\d+$"}}));

    let reason = json_body_reject_reason(&actual, &matcher);
    assert!(matches!(
        reason,
        Some(RejectReason::JsonRegexMismatch { .. })
    ));
}

#[test]
fn test_invalid_dollar_regex_reports_invalid_regex() {
    let actual = serde_json::json!({"code": "abc"});
    let matcher = expected(serde_json::json!({"code": {"$regex": "(["}}));

    let reason = json_body_reject_reason(&actual, &matcher);
    match reason {
        Some(RejectReason::InvalidRegex { pattern }) => assert_eq!(pattern, "(["),
        other => panic!("expected InvalidRegex, got {:?}", other),
    }
}

#[test]
fn test_dollar_type_checks_each_json_type() {
    let cases: Vec<(serde_json::Value, &str, bool)> = vec![
        (serde_json::json!("hi"), "string", true),
        (serde_json::json!(42), "number", true),
        (serde_json::json!(4.5), "number", true),
        (serde_json::json!(true), "boolean", true),
        (serde_json::json!({}), "object", true),
        (serde_json::json!([]), "array", true),
        (serde_json::json!(null), "null", true),
        // Wrong-type cases
        (serde_json::json!("42"), "number", false),
        (serde_json::json!(42), "string", false),
        (serde_json::json!({}), "array", false),
        (serde_json::json!(null), "string", false),
    ];

    for (value, type_name, should_match) in cases {
        let actual = serde_json::json!({"field": value});
        let matcher = expected(serde_json::json!({"field": {"$type": type_name}}));
        assert_eq!(
            match_json_body(&actual, &matcher),
            should_match,
            "field value {:?} against $type {}",
            value,
            type_name
        );
    }
}

#[test]
fn test_dollar_type_unknown_name_rejects_with_reason() {
    let actual = serde_json::json!({"field": 1});
    let matcher = expected(serde_json::json!({"field": {"$type": "integer"}}));

    let reason = json_body_reject_reason(&actual, &matcher);
    match reason {
        Some(RejectReason::JsonTypeMismatch { field, expected }) => {
            assert_eq!(field, "field");
            assert_eq!(expected, "integer");
        }
        other => panic!("expected JsonTypeMismatch, got {:?}", other),
    }
}

#[test]
fn test_dollar_contains_matches_any_array_element_partially() {
    let actual = serde_json::json!({"items": [
        {"sku": "B-2", "qty": 1},
        {"sku": "A-1", "qty": 7}
    ]});
    let matcher = expected(serde_json::json!({
        "items": {"$contains": {"sku": "A-1"}}
    }));

    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_dollar_contains_combines_with_nested_matcher_forms() {
    let actual = serde_json::json!({"items": [{"sku": "A-1"}]});
    let matcher = expected(serde_json::json!({
        "items": {"$contains": {"sku": {"$regex": "^A-"}}}
    }));

    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_dollar_contains_rejects_when_no_element_matches() {
    let actual = serde_json::json!({"items": [{"sku": "B-2"}]});
    let matcher = expected(serde_json::json!({
        "items": {"$contains": {"sku": "A-1"}}
    }));

    let reason = json_body_reject_reason(&actual, &matcher);
    match reason {
        Some(RejectReason::JsonContainsMissing { field }) => assert_eq!(field, "items"),
        other => panic!("expected JsonContainsMissing, got {:?}", other),
    }
}

#[test]
fn test_dollar_contains_rejects_non_array_actual() {
    let actual = serde_json::json!({"items": {"sku": "A-1"}});
    let matcher = expected(serde_json::json!({"items": {"$contains": {"sku": "A-1"}}}));

    let reason = json_body_reject_reason(&actual, &matcher);
    assert!(matches!(
        reason,
        Some(RejectReason::JsonTypeMismatch { .. })
    ));
}

#[test]
fn test_literal_object_with_multiple_keys_stays_literal() {
    // Two keys: not a matcher form, compared literally (and rejecting).
    let actual = serde_json::json!({"field": {"kind": "real", "v": 1}});
    let matcher = expected(serde_json::json!({
        "field": {"kind": "$any", "v": 2}
    }));

    assert!(!match_json_body(&actual, &matcher));
}

#[test]
fn test_dollar_escaped_key_is_literal_multi_key() {
    // `$any` next to another key is a literal object, not a form.
    let actual = serde_json::json!({"field": {"$any": true}});
    let matcher = expected(serde_json::json!({"field": {"$any": true, "other": 1}}));

    // `$any` and `other` are compared literally: actual lacks `other`.
    assert!(!match_json_body(&actual, &matcher));
}

#[test]
fn test_unknown_dollar_form_matches_nothing_with_a_reason() {
    let actual = serde_json::json!({"field": 1});
    let matcher = expected(serde_json::json!({"field": {"$regx": "1"}}));

    let reason = json_body_reject_reason(&actual, &matcher);
    assert!(matches!(reason, Some(RejectReason::JsonFieldMismatch { field }) if field == "field"));
}

#[test]
fn test_nesting_forms_at_depth() {
    let actual = serde_json::json!({
        "order": {
            "customer": {"email": "a@b.co", "tier": "gold"},
            "amount": 1200
        }
    });
    let matcher = expected(serde_json::json!({
        "order": {
            "customer": {
                "email": {"$regex": ".+@.+"},
                "tier": {"$any": true}
            },
            "amount": {"$type": "number"}
        }
    }));

    assert!(match_json_body(&actual, &matcher));
}

#[test]
fn test_existing_literal_partial_behaviour_unchanged() {
    // The regression case: literal partial matching must do exactly what it
    // did before forms existed.
    let actual = serde_json::json!({"username": "admin", "extra": "field"});
    let matcher = expected(serde_json::json!({"username": "admin"}));
    assert!(match_json_body(&actual, &matcher));

    let wrong = serde_json::json!({"username": "user"});
    assert!(!match_json_body(&wrong, &matcher));
}
