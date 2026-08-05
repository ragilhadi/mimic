//! Response templating: interpolate `{{source.key}}` expressions inside a mock's
//! `response` JSON using data pulled from the incoming request (path params,
//! query params, headers, and the request body).

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
    // Matches any `{{ source.key }}` shape; unrecognized sources or missing
    // keys are resolved to an empty string rather than rejected here, so a
    // malformed/unknown template never leaks into the response verbatim.
    RE.get_or_init(|| Regex::new(r"\{\{\s*([a-zA-Z0-9_]+)\.([^{}]+?)\s*\}\}").unwrap())
}

/// Render `{{ }}` templates found anywhere in `value`, returning a new JSON value.
/// If `value` contains no template expressions, it is cloned as-is without
/// touching the regex engine.
pub fn render_response(value: &Value, ctx: &TemplateContext) -> Value {
    if !contains_template(value) {
        return value.clone();
    }
    render_value(value, ctx)
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
        Value::String(s) => Value::String(interpolate(s, ctx)),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), render_value(v, ctx)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(|v| render_value(v, ctx)).collect()),
        other => other.clone(),
    }
}

fn interpolate(s: &str, ctx: &TemplateContext) -> String {
    if !s.contains("{{") {
        return s.to_string();
    }

    template_regex()
        .replace_all(s, |caps: &regex::Captures| {
            resolve(&caps[1], caps[2].trim(), ctx).unwrap_or_default()
        })
        .into_owned()
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
