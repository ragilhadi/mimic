//! Response templating: interpolate `{{source.key}}` expressions inside a mock's
//! `response` JSON using data pulled from the incoming request (path params,
//! query params, headers, and the request body), plus the request-independent
//! `{{faker.*}}` generators in [`crate::faker`].

use crate::matcher::ParsedBody;
use crate::types::is_sensitive_header;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;
use tracing::debug;

/// Data available to templates, sourced from the matched request.
pub struct TemplateContext<'a> {
    pub path_params: &'a HashMap<String, String>,
    pub query_params: &'a HashMap<String, String>,
    pub headers: &'a HashMap<String, String>,
    pub body: Option<&'a ParsedBody>,
}

fn template_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Matches any `{{ [cast:]source.key }}` shape; unrecognized sources or
    // missing keys are resolved to an empty string rather than rejected here,
    // so a malformed/unknown template never leaks into the response verbatim.
    // The optional cast prefix is deliberately any identifier, not just
    // `number|bool|json` — an unrecognized cast word (e.g. `{{foo:path.id}}`)
    // is meant to fall through to the same "resolves to empty" treatment as
    // an unrecognized source, so it's checked in code rather than the regex.
    RE.get_or_init(|| {
        Regex::new(r"\{\{\s*(?:([a-zA-Z0-9_]+)\s*:\s*)?([a-zA-Z0-9_]+)\.([^{}]+?)\s*\}\}").unwrap()
    })
}

/// Casts recognized by the `{{cast:source.key}}` syntax. Anything else in the
/// cast position is treated as an unrecognized/malformed template.
const KNOWN_CASTS: &[&str] = &["number", "bool", "json"];

/// Render `{{ }}` templates found anywhere in `value`, returning a new JSON value.
/// If `value` contains no template expressions, it is cloned as-is without
/// touching the regex engine.
pub fn render_response(value: &Value, ctx: &TemplateContext) -> Value {
    if !contains_template(value) {
        return value.clone();
    }
    render_value(value, ctx)
}

/// True if any template inside `value` reads from the request body, i.e. uses
/// a `{{body.…}}` expression.
///
/// Used before the body is read to decide whether a mock's response actually
/// needs it — a response that never mentions `{{body.…}}` doesn't.
pub fn references_body(value: &Value) -> bool {
    match value {
        Value::String(s) => text_references_body(s),
        Value::Object(map) => map.values().any(references_body),
        Value::Array(arr) => arr.iter().any(references_body),
        _ => false,
    }
}

/// Render `{{ }}` templates in a plain string — the text of a `response_file`
/// body rather than a JSON `response`.
///
/// Same expressions, same resolution rules, same "unknown source resolves to
/// empty" behavior as [`render_response`]; a string with no `{{` is returned
/// untouched without the regex engine seeing it.
pub fn render_text(text: &str, ctx: &TemplateContext) -> String {
    interpolate(text, ctx)
}

/// True if `text` uses a `{{body.…}}` expression — the string-level counterpart
/// of [`references_body`], used to decide whether a templated file body needs
/// the request body read for it.
pub fn text_references_body(text: &str) -> bool {
    text.contains("{{")
        && template_regex()
            .captures_iter(text)
            .any(|caps| &caps[2] == "body")
}

fn contains_template(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("{{"),
        Value::Object(map) => map.values().any(contains_template),
        Value::Array(arr) => arr.iter().any(contains_template),
        _ => false,
    }
}

fn render_value(value: &Value, ctx: &TemplateContext) -> Value {
    match value {
        Value::String(s) => render_string(s, ctx),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), render_value(v, ctx)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(|v| render_value(v, ctx)).collect()),
        other => other.clone(),
    }
}

/// Render a single string value, taking the typed-cast fast path when `s` is
/// *exactly* one `{{cast:source.key}}` expression, and falling back to plain
/// string interpolation otherwise (no cast, or a cast embedded in a larger
/// string that can't become anything but text).
fn render_string(s: &str, ctx: &TemplateContext) -> Value {
    if let Some(caps) = whole_string_template(s) {
        if let Some(cast) = caps.get(1) {
            let source = &caps[2];
            let key = caps[3].trim();
            return resolve_typed(source, key, cast.as_str(), ctx);
        }
    }
    Value::String(interpolate(s, ctx))
}

/// If `s` is made up of a single template expression spanning its entire
/// length, return the captures for it; otherwise `None`.
fn whole_string_template(s: &str) -> Option<regex::Captures<'_>> {
    let caps = template_regex().captures(s)?;
    let m = caps.get(0)?;
    (m.start() == 0 && m.end() == s.len()).then_some(caps)
}

fn interpolate(s: &str, ctx: &TemplateContext) -> String {
    if !s.contains("{{") {
        return s.to_string();
    }

    template_regex()
        .replace_all(s, |caps: &regex::Captures| match caps.get(1) {
            // A cast can't survive being spliced into the middle of a larger
            // string, so it's ignored here and the source resolves exactly as
            // it would unprefixed — except an unrecognized cast word, which
            // (like an unrecognized source) resolves to empty.
            Some(cast) if !KNOWN_CASTS.contains(&cast.as_str()) => String::new(),
            _ => resolve(&caps[2], caps[3].trim(), ctx).unwrap_or_default(),
        })
        .into_owned()
}

/// Resolve a `{{cast:source.key}}` expression to a typed JSON value, reusing
/// [`resolve`] so the cast and plain-interpolation paths can't disagree about
/// what a source means.
fn resolve_typed(source: &str, key: &str, cast: &str, ctx: &TemplateContext) -> Value {
    let raw = resolve(source, key, ctx);
    match cast {
        "number" => match raw {
            Some(s) => parse_number(&s).unwrap_or(Value::String(s)),
            None => Value::String(String::new()),
        },
        "bool" => match raw.as_deref() {
            Some("true") => Value::Bool(true),
            Some("false") => Value::Bool(false),
            Some(s) => Value::String(s.to_string()),
            None => Value::String(String::new()),
        },
        "json" => match raw {
            Some(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
            None => Value::Null,
        },
        // Unreachable via `render_string`/`interpolate`, which only ever pass
        // a cast from `KNOWN_CASTS`; kept as a safe default rather than
        // panicking if that invariant ever slips.
        _ => Value::String(String::new()),
    }
}

/// Parse a resolved string as a JSON number, trying an integer before falling
/// back to a float so whole numbers don't grow a spurious `.0`.
fn parse_number(s: &str) -> Option<Value> {
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::from(i));
    }
    s.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

fn resolve(source: &str, key: &str, ctx: &TemplateContext) -> Option<String> {
    match source {
        "path" => ctx.path_params.get(key).cloned(),
        "query" => ctx.query_params.get(key).cloned(),
        // Credential-bearing headers are never interpolated: echoing a live
        // bearer token or session cookie back into a response body would put
        // it in devtools, HAR exports and CI logs. They resolve like a
        // missing key does — to an empty string.
        "header" => {
            let name = key.to_lowercase();
            if is_sensitive_header(&name) {
                debug!("Refusing to interpolate sensitive header '{}'", name);
                None
            } else {
                ctx.headers.get(&name).cloned()
            }
        }
        "body" => resolve_body(ctx.body, key),
        // Request-independent generators: `{{faker.uuid}}`, `{{faker.int min=1 max=9}}`, …
        // Each occurrence is resolved on its own, so repeated expressions vary.
        "faker" => crate::faker::resolve(key),
        _ => None,
    }
}

fn resolve_body(body: Option<&ParsedBody>, key: &str) -> Option<String> {
    match body? {
        ParsedBody::Json(root) => {
            let mut current = root;
            for part in key.split('.') {
                current = current.get(part)?;
            }
            value_to_template_string(current)
        }
        ParsedBody::Form(fields) => fields.get(key).cloned(),
        _ => None,
    }
}

/// Missing keys and explicit JSON `null` both render as an empty string;
/// everything else uses its natural (unquoted for strings) text form.
fn value_to_template_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx<'a>(
        path_params: &'a HashMap<String, String>,
        query_params: &'a HashMap<String, String>,
        headers: &'a HashMap<String, String>,
        body: Option<&'a ParsedBody>,
    ) -> TemplateContext<'a> {
        TemplateContext {
            path_params,
            query_params,
            headers,
            body,
        }
    }

    #[test]
    fn test_no_template_is_zero_touch_clone() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);
        let value = json!({"message": "static response"});
        assert_eq!(render_response(&value, &c), value);
    }

    #[test]
    fn test_path_param_interpolation() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "42".to_string());
        let empty = HashMap::new();
        let c = ctx(&path_params, &empty, &empty, None);

        let value = json!({"id": "{{path.id}}"});
        assert_eq!(render_response(&value, &c), json!({"id": "42"}));
    }

    #[test]
    fn test_query_param_interpolation() {
        let mut query_params = HashMap::new();
        query_params.insert("page".to_string(), "2".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &query_params, &empty, None);

        let value = json!({"page": "{{query.page}}"});
        assert_eq!(render_response(&value, &c), json!({"page": "2"}));
    }

    #[test]
    fn test_header_interpolation_is_case_insensitive() {
        let mut headers = HashMap::new();
        headers.insert("x-request-id".to_string(), "abc-123".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &headers, None);

        let value = json!({"request_id": "{{header.X-Request-Id}}"});
        assert_eq!(
            render_response(&value, &c),
            json!({"request_id": "abc-123"})
        );
    }

    #[test]
    fn test_body_top_level_field_interpolation() {
        let body = ParsedBody::Json(json!({"username": "alice"}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"username": "{{body.username}}"});
        assert_eq!(render_response(&value, &c), json!({"username": "alice"}));
    }

    #[test]
    fn test_body_nested_dot_notation() {
        let body = ParsedBody::Json(json!({"user": {"email": "alice@example.com"}}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"email": "{{body.user.email}}"});
        assert_eq!(
            render_response(&value, &c),
            json!({"email": "alice@example.com"})
        );
    }

    #[test]
    fn test_body_form_field_interpolation() {
        let mut fields = HashMap::new();
        fields.insert("username".to_string(), "bob".to_string());
        let body = ParsedBody::Form(fields);
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"username": "{{body.username}}"});
        assert_eq!(render_response(&value, &c), json!({"username": "bob"}));
    }

    #[test]
    fn test_missing_variable_becomes_empty_string_no_panic() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"id": "{{path.id}}", "q": "{{query.missing}}"});
        assert_eq!(render_response(&value, &c), json!({"id": "", "q": ""}));
    }

    #[test]
    fn test_missing_body_field_becomes_empty_string() {
        let body = ParsedBody::Json(json!({"username": "alice"}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"email": "{{body.email}}"});
        assert_eq!(render_response(&value, &c), json!({"email": ""}));
    }

    #[test]
    fn test_body_null_field_becomes_empty_string() {
        let body = ParsedBody::Json(json!({"middle_name": null}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"middle_name": "{{body.middle_name}}"});
        assert_eq!(render_response(&value, &c), json!({"middle_name": ""}));
    }

    #[test]
    fn test_no_body_present_resolves_to_empty_string() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"username": "{{body.username}}"});
        assert_eq!(render_response(&value, &c), json!({"username": ""}));
    }

    #[test]
    fn test_multiple_templates_in_one_string() {
        let mut headers = HashMap::new();
        headers.insert("x-actor".to_string(), "admin".to_string());
        let body = ParsedBody::Json(json!({"username": "alice"}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &headers, Some(&body));

        let value = json!({"summary": "{{body.username}} created by {{header.x-actor}}"});
        assert_eq!(
            render_response(&value, &c),
            json!({"summary": "alice created by admin"})
        );
    }

    #[test]
    fn test_numeric_and_boolean_body_field_renders_unquoted() {
        let body = ParsedBody::Json(json!({"age": 30, "active": true}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"summary": "age={{body.age}} active={{body.active}}"});
        assert_eq!(
            render_response(&value, &c),
            json!({"summary": "age=30 active=true"})
        );
    }

    #[test]
    fn test_templates_inside_nested_arrays_and_objects() {
        let mut query_params = HashMap::new();
        query_params.insert("page".to_string(), "3".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &query_params, &empty, None);

        let value = json!({
            "meta": {"page": "{{query.page}}"},
            "tags": ["{{query.page}}", "static"]
        });
        assert_eq!(
            render_response(&value, &c),
            json!({
                "meta": {"page": "3"},
                "tags": ["3", "static"]
            })
        );
    }

    #[test]
    fn test_unrecognized_source_resolves_to_empty_string() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"x": "{{cookie.session}}"});
        assert_eq!(render_response(&value, &c), json!({"x": ""}));
    }

    #[test]
    fn test_sensitive_headers_are_never_interpolated() {
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            "Bearer sk_live_secret".to_string(),
        );
        headers.insert("cookie".to_string(), "session=abc123".to_string());
        headers.insert("set-cookie".to_string(), "session=abc123".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &headers, None);

        let value = json!({
            "auth": "{{header.authorization}}",
            "cookie": "{{header.cookie}}",
            "set_cookie": "{{header.set-cookie}}"
        });
        assert_eq!(
            render_response(&value, &c),
            json!({"auth": "", "cookie": "", "set_cookie": ""})
        );
    }

    #[test]
    fn test_sensitive_header_redaction_is_case_insensitive() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer secret".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &headers, None);

        let value = json!({"a": "{{header.Authorization}}", "b": "{{header.AUTHORIZATION}}"});
        assert_eq!(render_response(&value, &c), json!({"a": "", "b": ""}));
    }

    #[test]
    fn test_faker_uuid_and_bool_render_plausible_values() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"id": "{{faker.uuid}}", "verified": "{{faker.bool}}"});
        let rendered = render_response(&value, &c);

        let id = rendered["id"].as_str().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert!(matches!(
            rendered["verified"].as_str().unwrap(),
            "true" | "false"
        ));
    }

    #[test]
    fn test_faker_int_honours_min_and_max_args() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"score": "{{faker.int min=1 max=100}}"});
        for _ in 0..50 {
            let rendered = render_response(&value, &c);
            let score: i64 = rendered["score"].as_str().unwrap().parse().unwrap();
            assert!((1..=100).contains(&score), "{} out of range", score);
        }
    }

    #[test]
    fn test_faker_name_email_and_timestamp() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({
            "name": "{{faker.name}}",
            "email": "{{faker.email}}",
            "created_at": "{{faker.timestamp}}"
        });
        let rendered = render_response(&value, &c);

        assert!(rendered["name"].as_str().unwrap().contains(' '));
        assert!(rendered["email"]
            .as_str()
            .unwrap()
            .ends_with("@example.com"));
        chrono::DateTime::parse_from_rfc3339(rendered["created_at"].as_str().unwrap())
            .expect("timestamp is RFC 3339");
    }

    #[test]
    fn test_two_faker_occurrences_resolve_independently() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"a": "{{faker.uuid}}", "b": "{{faker.uuid}}"});
        let rendered = render_response(&value, &c);
        assert_ne!(rendered["a"], rendered["b"]);

        // …including two occurrences inside the same string.
        let value = json!({"pair": "{{faker.uuid}} {{faker.uuid}}"});
        let rendered = render_response(&value, &c);
        let pair = rendered["pair"].as_str().unwrap();
        let (first, second) = pair.split_once(' ').unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn test_faker_malformed_args_fall_back_to_default_range() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"n": "{{faker.int min=abc max=}}"});
        let rendered = render_response(&value, &c);
        let n: i64 = rendered["n"].as_str().unwrap().parse().unwrap();
        assert!((0..=1_000_000).contains(&n));
    }

    #[test]
    fn test_unknown_faker_generator_becomes_empty_string() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"x": "{{faker.credit_card}}"});
        assert_eq!(render_response(&value, &c), json!({"x": ""}));
    }

    #[test]
    fn test_faker_templates_do_not_imply_body_consumption() {
        // `references_body` decides whether the request body must be buffered;
        // faker needs nothing from the request.
        let value = json!({"id": "{{faker.uuid}}", "name": "{{faker.name}}"});
        assert!(!references_body(&value));
    }

    #[test]
    fn test_number_cast_produces_json_number() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "42".to_string());
        let empty = HashMap::new();
        let c = ctx(&path_params, &empty, &empty, None);

        let value = json!({"id": "{{number:path.id}}"});
        assert_eq!(render_response(&value, &c), json!({"id": 42}));
    }

    #[test]
    fn test_bool_cast_produces_json_boolean() {
        let mut query_params = HashMap::new();
        query_params.insert("active".to_string(), "true".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &query_params, &empty, None);

        let value = json!({"active": "{{bool:query.active}}"});
        assert_eq!(render_response(&value, &c), json!({"active": true}));
    }

    #[test]
    fn test_json_cast_produces_nested_object() {
        let body = ParsedBody::Json(json!({"user": {"id": 1, "name": "alice"}}));
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, Some(&body));

        let value = json!({"user": "{{json:body.user}}"});
        assert_eq!(
            render_response(&value, &c),
            json!({"user": {"id": 1, "name": "alice"}})
        );
    }

    #[test]
    fn test_number_cast_on_faker_int() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"views": "{{number:faker.int min=1 max=999}}"});
        let rendered = render_response(&value, &c);
        let views = rendered["views"].as_i64().expect("number, not string");
        assert!((1..=999).contains(&views));
    }

    #[test]
    fn test_cast_inside_larger_string_stays_plain_interpolation() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "42".to_string());
        let empty = HashMap::new();
        let c = ctx(&path_params, &empty, &empty, None);

        let value = json!({"slug": "user-{{number:path.id}}"});
        assert_eq!(render_response(&value, &c), json!({"slug": "user-42"}));
    }

    #[test]
    fn test_uncastable_number_falls_back_to_string_form() {
        let mut path_params = HashMap::new();
        path_params.insert("slug".to_string(), "abc".to_string());
        let empty = HashMap::new();
        let c = ctx(&path_params, &empty, &empty, None);

        let value = json!({"slug": "{{number:path.slug}}"});
        assert_eq!(render_response(&value, &c), json!({"slug": "abc"}));
    }

    #[test]
    fn test_uncastable_bool_falls_back_to_string_form() {
        let mut query_params = HashMap::new();
        query_params.insert("flag".to_string(), "maybe".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &query_params, &empty, None);

        let value = json!({"flag": "{{bool:query.flag}}"});
        assert_eq!(render_response(&value, &c), json!({"flag": "maybe"}));
    }

    #[test]
    fn test_invalid_json_cast_falls_back_to_string_form() {
        let mut query_params = HashMap::new();
        query_params.insert("tag".to_string(), "not-json".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &query_params, &empty, None);

        let value = json!({"tag": "{{json:query.tag}}"});
        assert_eq!(render_response(&value, &c), json!({"tag": "not-json"}));
    }

    #[test]
    fn test_missing_number_and_bool_cast_fall_back_to_empty_string() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"n": "{{number:query.missing}}", "b": "{{bool:query.missing}}"});
        assert_eq!(render_response(&value, &c), json!({"n": "", "b": ""}));
    }

    #[test]
    fn test_missing_json_cast_yields_null() {
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &empty, None);

        let value = json!({"user": "{{json:body.missing}}"});
        assert_eq!(render_response(&value, &c), json!({"user": null}));
    }

    #[test]
    fn test_unrecognized_cast_prefix_resolves_to_empty_string() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "42".to_string());
        let empty = HashMap::new();
        let c = ctx(&path_params, &empty, &empty, None);

        let value = json!({"id": "{{uuid:path.id}}"});
        assert_eq!(render_response(&value, &c), json!({"id": ""}));
    }

    #[test]
    fn test_references_body_detects_json_cast() {
        let value = json!({"user": "{{json:body.user}}"});
        assert!(references_body(&value));

        let text = "{{json:body.user}}";
        assert!(text_references_body(text));
    }

    #[test]
    fn test_number_cast_in_sequence_style_response_object() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "7".to_string());
        let empty = HashMap::new();
        let c = ctx(&path_params, &empty, &empty, None);

        let value = json!({"step": {"id": "{{number:path.id}}", "done": "{{bool:query.done}}"}});
        assert_eq!(
            render_response(&value, &c),
            json!({"step": {"id": 7, "done": ""}})
        );
    }

    #[test]
    fn test_existing_templates_unaffected_by_cast_support() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "42".to_string());
        let mut query_params = HashMap::new();
        query_params.insert("page".to_string(), "2".to_string());
        let mut headers = HashMap::new();
        headers.insert("x-actor".to_string(), "admin".to_string());
        let body = ParsedBody::Json(json!({"username": "alice", "age": 30}));
        let c = ctx(&path_params, &query_params, &headers, Some(&body));

        let value = json!({
            "id": "{{path.id}}",
            "page": "{{query.page}}",
            "actor": "{{header.x-actor}}",
            "username": "{{body.username}}",
            "summary": "{{body.username}} is {{body.age}}"
        });
        assert_eq!(
            render_response(&value, &c),
            json!({
                "id": "42",
                "page": "2",
                "actor": "admin",
                "username": "alice",
                "summary": "alice is 30"
            })
        );
    }

    #[test]
    fn test_non_sensitive_headers_still_interpolate() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer secret".to_string());
        headers.insert("x-tenant".to_string(), "acme".to_string());
        let empty = HashMap::new();
        let c = ctx(&empty, &empty, &headers, None);

        let value = json!({"tenant": "{{header.x-tenant}}"});
        assert_eq!(render_response(&value, &c), json!({"tenant": "acme"}));
    }
}
