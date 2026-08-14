//! Built-in CORS: an origin allowlist applied to every mock response, plus
//! automatic answers for the `OPTIONS` preflights a browser sends first.
//!
//! Off unless `MIMIC_CORS` is switched on. With it unset, nothing here runs
//! and responses are byte-for-byte what they were before this module existed —
//! which has to hold, or enabling a *feature* would silently rewrite what every
//! already-written mock returns.
//!
//! Two rules shape the rest:
//!
//! - **A mock's own header wins.** Every header this module emits is written
//!   only when the response doesn't already carry it, so a mock that sets
//!   `Access-Control-Allow-Origin` deliberately — to reproduce a CORS *failure*
//!   in a test — still gets exactly what it asked for.
//! - **A disallowed origin gets no header,** rather than a wrong one. That is
//!   what makes the browser's own console message name the real problem.
//!
//! This is hand-rolled rather than `tower_http::cors` so both of those rules,
//! and the request-log entry for a preflight, stay inside Mimic's own code
//! path — and so the binary doesn't grow a dependency for six headers.

use crate::matcher::RequestContext;
use crate::types::is_truthy;
use axum::http::{
    header::{
        ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, VARY,
    },
    HeaderMap, HeaderName, HeaderValue,
};
use tracing::warn;

/// Master switch. Unset or falsey means every other `MIMIC_CORS_*` variable is
/// ignored.
pub const CORS_ENV: &str = "MIMIC_CORS";
/// Comma-separated origin allowlist, or `*`.
const ORIGINS_ENV: &str = "MIMIC_CORS_ORIGINS";
/// Comma-separated methods advertised on a preflight.
const METHODS_ENV: &str = "MIMIC_CORS_METHODS";
/// Comma-separated request headers to allow, or `*` to reflect whatever the
/// preflight asked for.
const HEADERS_ENV: &str = "MIMIC_CORS_HEADERS";
/// Whether to send `Access-Control-Allow-Credentials`.
const CREDENTIALS_ENV: &str = "MIMIC_CORS_CREDENTIALS";
/// How long a browser may cache a preflight, in seconds.
const MAX_AGE_ENV: &str = "MIMIC_CORS_MAX_AGE";

/// Methods advertised when `MIMIC_CORS_METHODS` is unset.
const DEFAULT_METHODS: &str = "GET,POST,PUT,PATCH,DELETE,OPTIONS";
/// `Access-Control-Max-Age` when `MIMIC_CORS_MAX_AGE` is unset, in seconds.
const DEFAULT_MAX_AGE: u64 = 600;

/// Request headers naming a preflight's intent. Read straight off
/// [`RequestContext::headers`], which is keyed lowercase.
const REQUEST_METHOD_HEADER: &str = "access-control-request-method";
const REQUEST_HEADERS_HEADER: &str = "access-control-request-headers";
const ORIGIN_HEADER: &str = "origin";

/// What the request log records for a request Mimic answered as a preflight.
///
/// Preflights are logged rather than dropped: a dashboard that showed nothing
/// for a request the browser definitely sent would look like Mimic lost it.
pub const PREFLIGHT_EXPLANATION: &str = "answered as a CORS preflight";

/// Which origins may read a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginRule {
    /// `*` — any origin. Sent literally, unless credentials are enabled (see
    /// [`CorsConfig::allow_origin`]).
    Any,
    /// An explicit allowlist. An origin not on it gets no header at all.
    List(Vec<String>),
}

/// Which request headers a preflight may declare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderRule {
    /// `*` — echo back whatever `Access-Control-Request-Headers` asked for.
    Reflect,
    /// An explicit list, sent verbatim on every preflight.
    List(Vec<String>),
}

/// The resolved `MIMIC_CORS_*` configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsConfig {
    pub origins: OriginRule,
    pub methods: Vec<String>,
    pub headers: HeaderRule,
    pub credentials: bool,
    pub max_age: u64,
}

impl CorsConfig {
    /// Parse a configuration from `lookup`, or `None` when CORS is switched
    /// off.
    ///
    /// Takes the environment as a closure so the parsing rules can be tested
    /// without mutating the process environment, which every other test in the
    /// suite shares.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let switch = lookup(CORS_ENV)?;
        let switch = switch.trim();
        if switch.is_empty() {
            return None;
        }
        if !is_truthy(switch) {
            // A value that is neither truthy nor a recognizable "off" is
            // almost certainly a typo, and silently serving no CORS headers is
            // the hardest version of this to debug from the browser side.
            if !matches!(
                switch.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ) {
                warn!(
                    "Invalid {}='{}', treating CORS as disabled (use true/false)",
                    CORS_ENV, switch
                );
            }
            return None;
        }

        let origins = match lookup(ORIGINS_ENV) {
            Some(raw) if !raw.trim().is_empty() => {
                let entries = split_list(&raw);
                if entries.iter().any(|entry| entry == "*") {
                    OriginRule::Any
                } else {
                    OriginRule::List(entries)
                }
            }
            _ => OriginRule::Any,
        };

        let methods = match lookup(METHODS_ENV) {
            Some(raw) if !raw.trim().is_empty() => split_list(&raw),
            _ => split_list(DEFAULT_METHODS),
        }
        .into_iter()
        .map(|m| m.to_uppercase())
        .collect();

        let headers = match lookup(HEADERS_ENV) {
            Some(raw) if !raw.trim().is_empty() => {
                let entries = split_list(&raw);
                if entries.iter().any(|entry| entry == "*") {
                    HeaderRule::Reflect
                } else {
                    HeaderRule::List(entries)
                }
            }
            _ => HeaderRule::Reflect,
        };

        let credentials = lookup(CREDENTIALS_ENV).is_some_and(|raw| is_truthy(&raw));

        let max_age = match lookup(MAX_AGE_ENV) {
            Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<u64>() {
                Ok(n) => n,
                Err(_) => {
                    warn!(
                        "Invalid {}='{}', falling back to {} seconds",
                        MAX_AGE_ENV, raw, DEFAULT_MAX_AGE
                    );
                    DEFAULT_MAX_AGE
                }
            },
            _ => DEFAULT_MAX_AGE,
        };

        let config = CorsConfig {
            origins,
            methods,
            headers,
            credentials,
            max_age,
        };
        config.warn_about_conflicts();
        Some(config)
    }

    /// Log the one configuration a browser will reject outright.
    ///
    /// `Access-Control-Allow-Origin: *` together with
    /// `Access-Control-Allow-Credentials: true` is invalid per the Fetch
    /// standard; [`Self::allow_origin`] reflects the request origin instead, so
    /// the setup still works — but silently doing something other than what the
    /// config says is worth one line at startup.
    fn warn_about_conflicts(&self) {
        if self.credentials && self.origins == OriginRule::Any {
            warn!(
                "{}=* with {}=true is invalid per the CORS spec; reflecting the request origin instead",
                ORIGINS_ENV, CREDENTIALS_ENV
            );
        }
    }

    /// The `Access-Control-Allow-Origin` value for a request from `origin`, or
    /// `None` when that origin isn't allowed to read the response.
    pub fn allow_origin(&self, origin: Option<&str>) -> Option<String> {
        match &self.origins {
            // `*` and credentials can't be combined, so the wildcard becomes
            // "reflect whoever asked" — the spec-legal way to say the same
            // thing.
            OriginRule::Any if self.credentials => origin.map(str::to_string),
            OriginRule::Any => Some("*".to_string()),
            OriginRule::List(allowed) => origin
                .filter(|o| allowed.iter().any(|a| a.eq_ignore_ascii_case(o)))
                .map(str::to_string),
        }
    }

    /// True when the emitted `Access-Control-Allow-Origin` depends on who
    /// asked, and the response therefore must not be cached across origins.
    fn origin_dependent(&self) -> bool {
        self.credentials || matches!(self.origins, OriginRule::List(_))
    }
}

/// The configured CORS settings, or `None` when `MIMIC_CORS` is off.
///
/// Read once from the environment on first use, like the other
/// `MIMIC_*` limits, so a per-request lookup never touches `std::env`.
pub fn configured() -> Option<&'static CorsConfig> {
    static CONFIG: std::sync::OnceLock<Option<CorsConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| CorsConfig::from_env(|key| std::env::var(key).ok()))
        .as_ref()
}

/// Split a comma-separated env value, dropping blanks and surrounding space.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `Origin` this request came from, if any.
pub fn request_origin(context: &RequestContext) -> Option<&str> {
    context.headers.get(ORIGIN_HEADER).map(String::as_str)
}

/// The method a preflight is asking about, or `None` if this isn't one.
///
/// A preflight is an `OPTIONS` carrying `Access-Control-Request-Method`. A bare
/// `OPTIONS` — from curl, or a health probe — isn't one, and is left to fall
/// through to the ordinary 404 it has always received.
pub fn preflight_method(context: &RequestContext) -> Option<&str> {
    if !context.method.eq_ignore_ascii_case("OPTIONS") {
        return None;
    }
    context
        .headers
        .get(REQUEST_METHOD_HEADER)
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
}

/// The headers to answer a preflight with.
///
/// The status (204) and the decision that this request *is* a preflight with a
/// real endpoint behind it both belong to the handler; this builds the header
/// set once that's settled.
pub fn preflight_headers(config: &CorsConfig, context: &RequestContext) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let origin = request_origin(context);

    apply_headers(config, origin, &mut headers);

    set_if_absent(
        &mut headers,
        ACCESS_CONTROL_ALLOW_METHODS,
        &config.methods.join(", "),
    );

    let allow_headers = match &config.headers {
        HeaderRule::List(list) => Some(list.join(", ")),
        // Reflecting is what makes `*` usable with credentials, where a literal
        // `*` is not allowed — and it answers the exact request rather than
        // guessing at a list.
        HeaderRule::Reflect => {
            append_vary(&mut headers, "Access-Control-Request-Headers");
            context
                .headers
                .get(REQUEST_HEADERS_HEADER)
                .map(|requested| requested.trim().to_string())
                .filter(|requested| !requested.is_empty())
        }
    };
    if let Some(value) = allow_headers {
        set_if_absent(&mut headers, ACCESS_CONTROL_ALLOW_HEADERS, &value);
    }

    set_if_absent(
        &mut headers,
        ACCESS_CONTROL_MAX_AGE,
        &config.max_age.to_string(),
    );

    headers
}

/// Add the CORS headers an ordinary (non-preflight) response carries, leaving
/// any the mock already set untouched.
///
/// Only the origin-related headers belong here: `Access-Control-Allow-Methods`,
/// `-Headers`, and `-Max-Age` are answers to a preflight and are ignored by a
/// browser on a real response.
pub fn apply_headers(config: &CorsConfig, origin: Option<&str>, headers: &mut HeaderMap) {
    if config.origin_dependent() {
        append_vary(headers, "Origin");
    }

    let Some(allow_origin) = config.allow_origin(origin) else {
        // Disallowed origin: deliberately no header, so the browser reports a
        // CORS rejection rather than a mismatched allowance.
        return;
    };
    set_if_absent(headers, ACCESS_CONTROL_ALLOW_ORIGIN, &allow_origin);
    if config.credentials {
        set_if_absent(headers, ACCESS_CONTROL_ALLOW_CREDENTIALS, "true");
    }
}

/// Write `name: value` unless the response already carries `name`.
///
/// This single check is the "a mock's own header always wins" rule.
fn set_if_absent(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if headers.contains_key(&name) {
        return;
    }
    match HeaderValue::from_str(value) {
        Ok(parsed) => {
            headers.insert(name, parsed);
        }
        Err(_) => warn!("Skipping invalid CORS header value for {}: {}", name, value),
    }
}

/// Add `value` to `Vary` unless it's already listed.
fn append_vary(headers: &mut HeaderMap, value: &str) {
    let already_listed = headers.get_all(VARY).iter().any(|existing| {
        existing.to_str().is_ok_and(|existing| {
            existing
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(value))
        })
    });
    if already_listed {
        return;
    }
    if let Ok(parsed) = HeaderValue::from_str(value) {
        headers.append(VARY, parsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `lookup` closure over a fixed set of variables.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn context(method: &str, headers: &[(&str, &str)]) -> RequestContext {
        let mut context = RequestContext::new(method.to_string(), "/users".to_string());
        context.headers = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        context
    }

    fn header(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .map(|v| v.to_str().unwrap_or_default().to_string())
    }

    // ------------------------------------------------------------------
    // Configuration parsing
    // ------------------------------------------------------------------

    #[test]
    fn test_disabled_when_unset() {
        assert_eq!(CorsConfig::from_env(env(&[])), None);
    }

    #[test]
    fn test_disabled_when_falsey() {
        for value in ["false", "0", "no", "off", ""] {
            assert_eq!(
                CorsConfig::from_env(env(&[(CORS_ENV, value)])),
                None,
                "{} should leave CORS off",
                value
            );
        }
    }

    #[test]
    fn test_unparsable_switch_leaves_cors_off() {
        assert_eq!(CorsConfig::from_env(env(&[(CORS_ENV, "maybe")])), None);
    }

    #[test]
    fn test_defaults() {
        let config = CorsConfig::from_env(env(&[(CORS_ENV, "true")])).unwrap();

        assert_eq!(config.origins, OriginRule::Any);
        assert_eq!(config.headers, HeaderRule::Reflect);
        assert!(!config.credentials);
        assert_eq!(config.max_age, DEFAULT_MAX_AGE);
        assert_eq!(
            config.methods,
            vec!["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
        );
    }

    #[test]
    fn test_explicit_configuration() {
        let config = CorsConfig::from_env(env(&[
            (CORS_ENV, "1"),
            (
                ORIGINS_ENV,
                " http://localhost:3000 , http://localhost:5173 ",
            ),
            (METHODS_ENV, "get,post"),
            (HEADERS_ENV, "content-type, authorization"),
            (CREDENTIALS_ENV, "yes"),
            (MAX_AGE_ENV, "60"),
        ]))
        .unwrap();

        assert_eq!(
            config.origins,
            OriginRule::List(vec![
                "http://localhost:3000".to_string(),
                "http://localhost:5173".to_string()
            ])
        );
        assert_eq!(config.methods, vec!["GET", "POST"]);
        assert_eq!(
            config.headers,
            HeaderRule::List(vec![
                "content-type".to_string(),
                "authorization".to_string()
            ])
        );
        assert!(config.credentials);
        assert_eq!(config.max_age, 60);
    }

    #[test]
    fn test_invalid_max_age_falls_back() {
        let config =
            CorsConfig::from_env(env(&[(CORS_ENV, "true"), (MAX_AGE_ENV, "soon")])).unwrap();
        assert_eq!(config.max_age, DEFAULT_MAX_AGE);
    }

    #[test]
    fn test_wildcard_among_a_list_wins() {
        let config =
            CorsConfig::from_env(env(&[(CORS_ENV, "true"), (ORIGINS_ENV, "http://a,*")])).unwrap();
        assert_eq!(config.origins, OriginRule::Any);
    }

    // ------------------------------------------------------------------
    // Origin resolution
    // ------------------------------------------------------------------

    #[test]
    fn test_wildcard_origin_is_sent_literally() {
        let config = CorsConfig::from_env(env(&[(CORS_ENV, "true")])).unwrap();
        assert_eq!(
            config.allow_origin(Some("http://localhost:3000")),
            Some("*".to_string())
        );
        assert_eq!(config.allow_origin(None), Some("*".to_string()));
    }

    #[test]
    fn test_wildcard_with_credentials_reflects_the_origin() {
        let config =
            CorsConfig::from_env(env(&[(CORS_ENV, "true"), (CREDENTIALS_ENV, "true")])).unwrap();
        assert_eq!(
            config.allow_origin(Some("http://localhost:3000")),
            Some("http://localhost:3000".to_string()),
            "`*` is invalid with credentials, so the request origin is reflected"
        );
        assert_eq!(config.allow_origin(None), None);
    }

    #[test]
    fn test_allowlist_admits_only_listed_origins() {
        let config = CorsConfig::from_env(env(&[
            (CORS_ENV, "true"),
            (ORIGINS_ENV, "http://localhost:3000"),
        ]))
        .unwrap();

        assert_eq!(
            config.allow_origin(Some("http://localhost:3000")),
            Some("http://localhost:3000".to_string())
        );
        assert_eq!(config.allow_origin(Some("http://evil.example")), None);
        assert_eq!(config.allow_origin(None), None);
    }

    // ------------------------------------------------------------------
    // Header emission
    // ------------------------------------------------------------------

    #[test]
    fn test_apply_headers_sets_allow_origin() {
        let config = CorsConfig::from_env(env(&[(CORS_ENV, "true")])).unwrap();
        let mut headers = HeaderMap::new();

        apply_headers(&config, Some("http://localhost:3000"), &mut headers);

        assert_eq!(
            header(&headers, "access-control-allow-origin"),
            Some("*".into())
        );
        assert_eq!(
            header(&headers, "vary"),
            None,
            "a wildcard answer is the same for everyone, so it needs no Vary"
        );
    }

    #[test]
    fn test_apply_headers_never_overwrites_a_mocks_own_header() {
        let config = CorsConfig::from_env(env(&[(CORS_ENV, "true")])).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("http://only-this-one"),
        );

        apply_headers(&config, Some("http://localhost:3000"), &mut headers);

        assert_eq!(
            header(&headers, "access-control-allow-origin"),
            Some("http://only-this-one".into())
        );
    }

    #[test]
    fn test_apply_headers_omits_the_header_for_a_disallowed_origin() {
        let config = CorsConfig::from_env(env(&[
            (CORS_ENV, "true"),
            (ORIGINS_ENV, "http://localhost:3000"),
        ]))
        .unwrap();
        let mut headers = HeaderMap::new();

        apply_headers(&config, Some("http://evil.example"), &mut headers);

        assert_eq!(header(&headers, "access-control-allow-origin"), None);
        assert_eq!(
            header(&headers, "vary"),
            Some("Origin".into()),
            "an allowlisted answer varies by origin even when it is a refusal"
        );
    }

    #[test]
    fn test_credentials_header() {
        let config = CorsConfig::from_env(env(&[
            (CORS_ENV, "true"),
            (ORIGINS_ENV, "http://localhost:3000"),
            (CREDENTIALS_ENV, "true"),
        ]))
        .unwrap();
        let mut headers = HeaderMap::new();

        apply_headers(&config, Some("http://localhost:3000"), &mut headers);

        assert_eq!(
            header(&headers, "access-control-allow-credentials"),
            Some("true".into())
        );
    }

    #[test]
    fn test_vary_is_not_duplicated() {
        let config = CorsConfig::from_env(env(&[
            (CORS_ENV, "true"),
            (ORIGINS_ENV, "http://localhost:3000"),
        ]))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(VARY, HeaderValue::from_static("Accept, origin"));

        apply_headers(&config, Some("http://localhost:3000"), &mut headers);

        assert_eq!(headers.get_all(VARY).iter().count(), 1);
    }

    // ------------------------------------------------------------------
    // Preflight detection and answers
    // ------------------------------------------------------------------

    #[test]
    fn test_preflight_method_requires_the_request_method_header() {
        assert_eq!(
            preflight_method(&context("OPTIONS", &[(REQUEST_METHOD_HEADER, "POST")])),
            Some("POST")
        );
        assert_eq!(
            preflight_method(&context("OPTIONS", &[])),
            None,
            "a bare OPTIONS is not a preflight"
        );
        assert_eq!(
            preflight_method(&context("GET", &[(REQUEST_METHOD_HEADER, "POST")])),
            None
        );
    }

    #[test]
    fn test_preflight_headers_reflect_requested_headers() {
        let config = CorsConfig::from_env(env(&[(CORS_ENV, "true")])).unwrap();
        let context = context(
            "OPTIONS",
            &[
                (ORIGIN_HEADER, "http://localhost:3000"),
                (REQUEST_METHOD_HEADER, "POST"),
                (REQUEST_HEADERS_HEADER, "content-type, authorization"),
            ],
        );

        let headers = preflight_headers(&config, &context);

        assert_eq!(
            header(&headers, "access-control-allow-origin"),
            Some("*".into())
        );
        assert_eq!(
            header(&headers, "access-control-allow-methods"),
            Some("GET, POST, PUT, PATCH, DELETE, OPTIONS".into())
        );
        assert_eq!(
            header(&headers, "access-control-allow-headers"),
            Some("content-type, authorization".into())
        );
        assert_eq!(
            header(&headers, "access-control-max-age"),
            Some("600".into())
        );
    }

    #[test]
    fn test_preflight_headers_use_a_configured_header_list() {
        let config =
            CorsConfig::from_env(env(&[(CORS_ENV, "true"), (HEADERS_ENV, "x-api-key")])).unwrap();
        let context = context(
            "OPTIONS",
            &[
                (REQUEST_METHOD_HEADER, "POST"),
                (REQUEST_HEADERS_HEADER, "content-type"),
            ],
        );

        let headers = preflight_headers(&config, &context);

        assert_eq!(
            header(&headers, "access-control-allow-headers"),
            Some("x-api-key".into()),
            "an explicit list is advertised as-is, not echoed back"
        );
    }

    #[test]
    fn test_preflight_headers_omit_allow_headers_when_none_were_requested() {
        let config = CorsConfig::from_env(env(&[(CORS_ENV, "true")])).unwrap();
        let context = context("OPTIONS", &[(REQUEST_METHOD_HEADER, "GET")]);

        let headers = preflight_headers(&config, &context);

        assert_eq!(header(&headers, "access-control-allow-headers"), None);
    }
}
