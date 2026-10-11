//! Request matching module for advanced mock matching.
//!
//! This module provides matching logic for:
//! - Query parameters
//! - HTTP headers
//! - Request body (JSON, text, form)

use crate::regex_cache::{cached, compiled_regex, CompileCache};
use crate::types::{
    is_sensitive_header, BodyMatcher, FormBodyMatcher, HeaderMatcher, HeaderPattern, HeaderValue,
    JsonBodyMatcher, MockConfig, QueryParamMatcher, QueryParamPattern, QueryParamValue,
    TextBodyMatcher,
};
use bytes::Bytes;
use regex::Regex;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, OnceLock};
use tracing::{debug, warn};

// ============================================================================
// Rejection Reasons - why a matcher turned a mock down
// ============================================================================

/// Placeholder substituted for any value belonging to a sensitive header.
const REDACTED: &str = "[REDACTED]";

/// Longest value echoed back inside a rejection reason, in characters.
const MAX_DISPLAY_LEN: usize = 80;

/// The first thing about a request that made a mock's matchers turn it down.
///
/// Produced by the `*_reject_reason` functions — which *are* the matchers,
/// with the `match_*` booleans defined as `reason.is_none()` — so an
/// explanation can never describe a rule the server doesn't enforce.
///
/// Values carried here are display-ready: already truncated, and already
/// redacted when they belong to a sensitive header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    QueryParamMissing {
        name: String,
    },
    QueryParamMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    /// `strict: true` and the request carried a query param the mock doesn't declare
    QueryParamExtra {
        name: String,
    },
    HeaderMissing {
        name: String,
    },
    HeaderMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    HeaderForbidden {
        name: String,
    },
    /// `strict: true` and the request carried a header the mock doesn't declare
    HeaderExtra {
        name: String,
    },
    /// The mock declares a body matcher, but no body was read for this request
    BodyAbsent,
    /// The mock expects an empty body and the request carried one
    BodyNotEmpty,
    /// The body couldn't be read as the kind the matcher expects
    BodyWrongKind {
        expected: &'static str,
    },
    JsonExact,
    JsonFieldMissing {
        field: String,
    },
    JsonFieldMismatch {
        field: String,
    },
    JsonFieldExtra {
        field: String,
    },
    JsonArrayLength {
        field: String,
        expected: usize,
        actual: usize,
    },
    TextExact,
    TextContains {
        substring: String,
    },
    TextRegex {
        pattern: String,
    },
    /// A configured regex failed to compile, so nothing can match it
    InvalidRegex {
        pattern: String,
    },
    FormFieldMissing {
        field: String,
    },
    FormFieldMismatch {
        field: String,
    },
    FormFieldExtra {
        field: String,
    },
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::QueryParamMissing { name } => {
                write!(f, "required query param `{}` was absent", name)
            }
            RejectReason::QueryParamMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "query param `{}` was `{}`, expected {}",
                name, actual, expected
            ),
            RejectReason::QueryParamExtra { name } => write!(
                f,
                "strict query matching rejected the undeclared param `{}`",
                name
            ),
            RejectReason::HeaderMissing { name } => {
                write!(f, "required header `{}` was absent", name)
            }
            RejectReason::HeaderMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "header `{}` was `{}`, expected {}",
                name, actual, expected
            ),
            RejectReason::HeaderForbidden { name } => {
                write!(f, "forbidden header `{}` was present", name)
            }
            RejectReason::HeaderExtra { name } => write!(
                f,
                "strict header matching rejected the undeclared header `{}`",
                name
            ),
            RejectReason::BodyAbsent => {
                write!(f, "the mock requires a request body and none was sent")
            }
            RejectReason::BodyNotEmpty => {
                write!(f, "the mock requires an empty body and one was sent")
            }
            RejectReason::BodyWrongKind { expected } => {
                write!(f, "the body could not be read as {}", expected)
            }
            RejectReason::JsonExact => write!(f, "the JSON body did not match `exact`"),
            RejectReason::JsonFieldMissing { field } => {
                write!(f, "JSON body is missing the field `{}`", field)
            }
            RejectReason::JsonFieldMismatch { field } => {
                write!(f, "JSON body field `{}` has a different value", field)
            }
            RejectReason::JsonFieldExtra { field } => write!(
                f,
                "strict JSON matching rejected the undeclared field `{}`",
                field
            ),
            RejectReason::JsonArrayLength {
                field,
                expected,
                actual,
            } => write!(
                f,
                "strict JSON matching expected {} item(s) in `{}`, got {}",
                expected, field, actual
            ),
            RejectReason::TextExact => write!(f, "the text body did not match `exact`"),
            RejectReason::TextContains { substring } => {
                write!(f, "the text body does not contain `{}`", substring)
            }
            RejectReason::TextRegex { pattern } => {
                write!(f, "the text body does not match the regex `{}`", pattern)
            }
            RejectReason::InvalidRegex { pattern } => {
                write!(f, "the configured regex `{}` is invalid", pattern)
            }
            RejectReason::FormFieldMissing { field } => {
                write!(f, "form body is missing the field `{}`", field)
            }
            RejectReason::FormFieldMismatch { field } => {
                write!(f, "form body field `{}` has a different value", field)
            }
            RejectReason::FormFieldExtra { field } => write!(
                f,
                "strict form matching rejected the undeclared field `{}`",
                field
            ),
        }
    }
}

/// Shorten `value` to [`MAX_DISPLAY_LEN`] characters (not bytes, so a
/// multi-byte character is never cut in half), marking anything dropped.
fn truncate_for_display(value: &str) -> String {
    match value.char_indices().nth(MAX_DISPLAY_LEN) {
        Some((byte_idx, _)) => format!("{}…", &value[..byte_idx]),
        None => value.to_string(),
    }
}

/// Describe the root of a dotted field path, which is empty when the mismatch
/// is the JSON document itself rather than a field within it.
fn describe_field_path(field_path: &str) -> String {
    if field_path.is_empty() {
        "(root)".to_string()
    } else {
        field_path.to_string()
    }
}

/// Render a query param expectation the way a human would read it.
fn describe_query_value(expected: &QueryParamValue) -> String {
    match expected {
        QueryParamValue::Exact(value) => format!("`{}`", truncate_for_display(value)),
        QueryParamValue::Pattern(QueryParamPattern::Regex(pattern)) => {
            format!("a match for `{}`", truncate_for_display(pattern))
        }
        QueryParamValue::Pattern(QueryParamPattern::Any) => "any value".to_string(),
        QueryParamValue::Pattern(QueryParamPattern::Contains(value)) => {
            format!("one of the values to be `{}`", truncate_for_display(value))
        }
        QueryParamValue::Pattern(QueryParamPattern::List(values)) => {
            format!("exactly {:?}", values)
        }
    }
}

/// Render a header expectation the way a human would read it.
fn describe_header_value(expected: &HeaderValue) -> String {
    match expected {
        HeaderValue::Exact(value) => format!("`{}`", truncate_for_display(value)),
        HeaderValue::Pattern(HeaderPattern::Regex(pattern)) => {
            format!("a match for `{}`", truncate_for_display(pattern))
        }
        HeaderValue::Pattern(HeaderPattern::Any) => "any value".to_string(),
        HeaderValue::Pattern(HeaderPattern::Prefix(prefix)) => {
            format!("a value starting with `{}`", truncate_for_display(prefix))
        }
        HeaderValue::Pattern(HeaderPattern::Contains(substring)) => {
            format!("a value containing `{}`", truncate_for_display(substring))
        }
    }
}

/// A map's entries, ordered by key.
///
/// `HashMap` iteration order is reseeded per process, so walking a matcher's
/// requirements directly would let two equally-failing rules take turns being
/// "the reason" across restarts. Explanations are compared and screenshotted
/// by humans; they need to be reproducible.
fn sorted_by_key<V>(map: &HashMap<String, V>) -> Vec<(&String, &V)> {
    let mut entries: Vec<(&String, &V)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// A map's keys, ordered — see [`sorted_by_key`].
fn sorted_keys<V>(map: &HashMap<String, V>) -> Vec<&String> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys
}

// ============================================================================
// Request Context - Parsed request data for matching
// ============================================================================

/// Parsed request data used for matching against mocks
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub method: String,
    pub path: String,
    /// Named path parameters (e.g. `:id`). Always empty until route-level
    /// path parameter matching is implemented; reserved so templating
    /// (`{{path.id}}`) has a source to read from once it lands.
    pub path_params: HashMap<String, String>,
    /// Each query parameter's *first* value — what `{{query.x}}` renders and
    /// what the ordinary `Exact`/`Regex`/`Any` matchers test against, so a
    /// mock written before repeated keys were supported keeps meaning exactly
    /// what it always did. See [`RequestContext::query_params_all`] for every
    /// value a repeated key carried.
    pub query_params: HashMap<String, String>,
    /// Every value each query parameter carried, in the order the client
    /// sent them — what the request log records and what
    /// [`crate::types::QueryParamPattern::Contains`]/`List` match against.
    pub query_params_all: HashMap<String, Vec<String>>,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
    pub content_type: Option<String>,
}

impl RequestContext {
    #[allow(dead_code)]
    pub fn new(method: String, path: String) -> Self {
        Self {
            method,
            path,
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            query_params_all: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            content_type: None,
        }
    }
}

// ============================================================================
// Query Parameter Parsing and Matching
// ============================================================================

/// Decode one `application/x-www-form-urlencoded` component — a query-string
/// key, a query-string value, or a form field.
///
/// `+` means space in form encoding. That is how every browser form post,
/// `URLSearchParams`, `curl --data-urlencode`, jQuery, and axios encode a
/// space, and how query strings are conventionally decoded too;
/// `urlencoding::decode` implements percent-decoding only and leaves the plus
/// alone.
///
/// The order matters and is the reason this is one function rather than two
/// lines at four call sites: `+` is replaced **first**, percent escapes
/// **second**. The reverse order would decode `%2B` — the encoding of a
/// literal plus — into `+` and then corrupt it into a space.
pub(crate) fn decode_component(raw: &str) -> String {
    // `replace` allocates unconditionally, so the common plus-free component
    // keeps borrowing the slice it came in on.
    let plus_decoded: Cow<'_, str> = if raw.contains('+') {
        Cow::Owned(raw.replace('+', " "))
    } else {
        Cow::Borrowed(raw)
    };

    urlencoding::decode(plus_decoded.as_ref())
        .unwrap_or_default()
        .into_owned()
}

/// Percent-decode a request path, one segment at a time.
///
/// Unlike a query string, a path has no `+`-means-space rule, so only `%XX`
/// escapes are decoded. Decoding happens per segment, after splitting on `/`,
/// and an encoded slash (`%2F`) is left encoded: `/files/a%2Fb` is one segment
/// named `a%2Fb`, and must not turn into the two-segment path `/files/a/b`. A
/// segment whose escapes don't decode to valid UTF-8 is kept as the client sent
/// it rather than guessed at.
pub(crate) fn decode_path(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_string();
    }

    raw.split('/')
        .map(decode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn decode_path_segment(segment: &str) -> String {
    if !segment.contains('%') {
        return segment.to_string();
    }

    // Split around `%2F`/`%2f` so the slash survives decoding.
    let mut pieces: Vec<String> = Vec::new();
    let mut rest = segment;
    loop {
        match rest.to_ascii_lowercase().find("%2f") {
            Some(at) => {
                pieces.push(rest[..at].to_string());
                rest = &rest[at + 3..];
            }
            None => {
                pieces.push(rest.to_string());
                break;
            }
        }
    }

    let mut decoded = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        match urlencoding::decode(piece) {
            Ok(text) => decoded.push(text.into_owned()),
            Err(_) => return segment.to_string(),
        }
    }
    decoded.join("%2F")
}

/// Parse a query string into every value each key carried, in the order the
/// client sent them.
/// Example: "tag=a&tag=b" -> {"tag": ["a", "b"]}
///
/// Keys and values arrive decoded: `?q=hello+world` and `?q=hello%20world`
/// both yield `hello world`. This is the source of truth the request log
/// reads, and the source [`parse_query_string`]'s first-value view is
/// derived from — see #108, which is what a repeated key needs to stop
/// silently losing every value but the last.
pub fn parse_query_string_multi(query: Option<&str>) -> HashMap<String, Vec<String>> {
    let mut params: HashMap<String, Vec<String>> = HashMap::new();

    if let Some(q) = query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }

            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (decode_component(key), decode_component(value)),
                // Handle params without values (e.g., ?debug)
                None => (decode_component(pair), String::new()),
            };
            params.entry(key).or_default().push(value);
        }
    }

    params
}

/// Each query parameter's first value.
/// Example: "page=1&limit=10" -> {"page": "1", "limit": "10"}
///
/// For a key sent more than once, this is deliberately the *first*
/// occurrence — see [`RequestContext::query_params`] — which is the form a
/// mock's ordinary matcher and `{{query.q}}` see.
pub fn parse_query_string(query: Option<&str>) -> HashMap<String, String> {
    parse_query_string_multi(query)
        .into_iter()
        .map(|(key, mut values)| {
            let first = if values.is_empty() {
                String::new()
            } else {
                values.remove(0)
            };
            (key, first)
        })
        .collect()
}

/// Check if request query params match the mock's requirements
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
pub fn match_query_params(
    request_params: &HashMap<String, String>,
    matcher: &QueryParamMatcher,
) -> bool {
    query_params_reject_reason(request_params, matcher).is_none()
}

/// [`query_params_reject_reason`], without every value a repeated key
/// carried. `Contains`/`List` fall back to treating each key's single value
/// as its whole (one-element) list — correct for a key sent once, and the
/// only case every existing caller of this function exercises.
///
/// This is what keeps [`match_query_params`] and every unit test that built a
/// plain `HashMap<String, String>` before repeated query params were matched
/// at all (#108) compiling and passing unchanged; [`evaluate_mock`] is the one
/// caller that has every value to give and calls
/// [`query_params_reject_reason_all`] directly.
pub fn query_params_reject_reason(
    request_params: &HashMap<String, String>,
    matcher: &QueryParamMatcher,
) -> Option<RejectReason> {
    query_params_reject_reason_all(request_params, None, matcher)
}

/// Why `matcher` rejects `request_params`, or `None` if it accepts them.
///
/// This is the query-parameter matcher; [`match_query_params`] is the boolean
/// view of it. Keeping one implementation is what stops the dashboard's
/// explanations from describing a matcher the server doesn't actually run.
///
/// `request_params_all`, when given, is every value each repeated key
/// carried — what `Contains` and `List` (#108) match against. Every other
/// matcher, and the `strict` extra-param check, reads `request_params`
/// exactly as before: one entry per key, so a repeated key is still counted
/// once under `strict`, not once per value.
pub fn query_params_reject_reason_all(
    request_params: &HashMap<String, String>,
    request_params_all: Option<&HashMap<String, Vec<String>>>,
    matcher: &QueryParamMatcher,
) -> Option<RejectReason> {
    // Check all required params match. Iterated in name order so a mock with
    // several failing params always reports the same one.
    for (key, expected_value) in sorted_by_key(&matcher.params) {
        match request_params.get(key) {
            Some(actual_value) => {
                let all_values = request_params_all.and_then(|all| all.get(key));
                if !match_query_param_value(actual_value, all_values, expected_value) {
                    debug!(
                        "Query param '{}' mismatch: expected {:?}, got '{}'",
                        key, expected_value, actual_value
                    );
                    return Some(RejectReason::QueryParamMismatch {
                        name: key.clone(),
                        expected: describe_query_value(expected_value),
                        actual: truncate_for_display(actual_value),
                    });
                }
            }
            None => {
                debug!("Required query param '{}' is missing", key);
                return Some(RejectReason::QueryParamMissing { name: key.clone() });
            }
        }
    }

    // If strict mode, check for extra params. `request_params` has one entry
    // per key regardless of how many times the client repeated it, so a
    // repeated key is counted once here, not once per value.
    if matcher.strict {
        for key in sorted_keys(request_params) {
            if !matcher.params.contains_key(key) {
                debug!("Strict mode: extra query param '{}' found", key);
                return Some(RejectReason::QueryParamExtra { name: key.clone() });
            }
        }
    }

    None
}

/// Match a single query parameter value.
///
/// `all_values` — every occurrence of this key, when the caller has it — is
/// only consulted by `Contains` and `List`; `Exact`/`Regex`/`Any` test
/// `actual` (the first value) exactly as they always have, so a key sent once
/// matches identically whether or not `all_values` is available.
fn match_query_param_value(
    actual: &str,
    all_values: Option<&Vec<String>>,
    expected: &QueryParamValue,
) -> bool {
    match expected {
        QueryParamValue::Exact(value) => actual == value,
        QueryParamValue::Pattern(pattern) => match pattern {
            QueryParamPattern::Regex(regex_str) => match compiled_regex(regex_str) {
                Some(re) => re.is_match(actual),
                None => false,
            },
            QueryParamPattern::Any => true,
            QueryParamPattern::Contains(value) => match all_values {
                Some(values) => values.iter().any(|v| v == value),
                None => actual == value,
            },
            QueryParamPattern::List(expected_list) => match all_values {
                Some(values) => values == expected_list,
                None => expected_list.len() == 1 && expected_list[0] == actual,
            },
        },
    }
}

// ============================================================================
// Header Parsing and Matching
// ============================================================================

/// Parse HTTP headers into HashMap (normalized to lowercase)
///
/// A header sent more than once — `X-Tag: a` and `X-Tag: b`, or the several
/// `Cookie` lines an HTTP/2 client splits one into — keeps every value, joined
/// with `, ` in the order received (RFC 9110 §5.3), instead of letting the last
/// one overwrite the rest. A value that isn't valid UTF-8 is kept lossily rather
/// than dropped: a matcher on a header the client really sent should see it,
/// not report it absent.
pub fn parse_headers(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    let mut parsed: HashMap<String, String> = HashMap::new();

    for (name, value) in headers.iter() {
        let name_str = name.as_str().to_lowercase();
        let value_str = match value.to_str() {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => String::from_utf8_lossy(value.as_bytes()),
        };

        match parsed.entry(name_str) {
            std::collections::hash_map::Entry::Occupied(mut existing) => {
                let joined = existing.get_mut();
                joined.push_str(", ");
                joined.push_str(&value_str);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(value_str.into_owned());
            }
        }
    }

    parsed
}

/// Headers that `strict: true` never counts as "extra".
///
/// These are sent unconditionally by mainstream HTTP clients (curl, browsers,
/// `fetch`, Postman, most libraries), so counting them would make strict mode
/// reject essentially all real traffic unless every mock declared them.
/// `accept` in particular is sent by curl (`Accept: */*`) and by every browser
/// on every request. The browser-specific entries here — `origin`, `referer`,
/// `accept-language`, `cache-control`, `pragma`, `dnt`,
/// `upgrade-insecure-requests` — are what a bare `fetch()` adds beyond what
/// curl sends; see [`IGNORED_HEADER_PREFIXES`] for the `sec-*` families
/// alongside them (#107).
pub const IGNORED_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "connection",
    "content-length",
    "dnt",
    "host",
    "origin",
    "pragma",
    "referer",
    "upgrade-insecure-requests",
    "user-agent",
];

/// Header name *prefixes* `strict: true` never counts as "extra".
///
/// A browser's `sec-fetch-*` and `sec-ch-ua*` families grow new members over
/// time (`sec-fetch-storage-access`, `sec-ch-ua-form-factors`, ...), and
/// enumerating each one by name is exactly what made #107 possible in the
/// first place. Matching the `sec-` prefix covers the whole family — and
/// anything else browsers introduce under it — without another release.
pub const IGNORED_HEADER_PREFIXES: &[&str] = &["sec-"];

/// Environment variable naming additional headers `strict: true` should
/// ignore, comma-separated — additive to [`IGNORED_HEADERS`], for a client
/// Mimic hasn't heard of yet.
pub const STRICT_IGNORE_HEADERS_ENV: &str = "MIMIC_STRICT_IGNORE_HEADERS";

/// `MIMIC_STRICT_IGNORE_HEADERS`, parsed once: lowercased header names to
/// additionally ignore under `strict: true`.
fn configured_strict_ignore_headers() -> &'static HashSet<String> {
    static EXTRA: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    EXTRA.get_or_init(|| {
        std::env::var(STRICT_IGNORE_HEADERS_ENV)
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(|entry| entry.trim().to_ascii_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// True when `strict: true` should never count `name` (already lowercase) as
/// an undeclared, "extra" header — it's one of [`IGNORED_HEADERS`], matches an
/// [`IGNORED_HEADER_PREFIXES`] family, or was added via
/// [`STRICT_IGNORE_HEADERS_ENV`].
fn is_strict_ignored_header(name: &str) -> bool {
    IGNORED_HEADERS.contains(&name)
        || IGNORED_HEADER_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || configured_strict_ignore_headers().contains(name)
}

/// Check if request headers match the mock's requirements
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
pub fn match_headers(request_headers: &HashMap<String, String>, matcher: &HeaderMatcher) -> bool {
    headers_reject_reason(request_headers, matcher).is_none()
}

/// Why `matcher` rejects `request_headers`, or `None` if it accepts them.
///
/// This is the header matcher; [`match_headers`] is the boolean view of it.
///
/// Header *values* — both the request's and the mock's expectation — are
/// redacted here, at construction, whenever the header is sensitive. The
/// reason value therefore never carries a credential in the first place, so
/// no downstream formatting or serialization can leak one.
pub fn headers_reject_reason(
    request_headers: &HashMap<String, String>,
    matcher: &HeaderMatcher,
) -> Option<RejectReason> {
    // Check all required headers match, in name order for a stable report
    for (name, expected_value) in sorted_by_key(&matcher.required) {
        let normalized_name = name.to_lowercase();

        match request_headers.get(&normalized_name) {
            Some(actual_value) => {
                if !match_header_value(actual_value, expected_value) {
                    debug!(
                        "Header '{}' mismatch: expected {:?}, got '{}'",
                        name, expected_value, actual_value
                    );
                    let sensitive = is_sensitive_header(&normalized_name);
                    return Some(RejectReason::HeaderMismatch {
                        name: normalized_name,
                        // Backticked to match the shape of a real expectation,
                        // so both halves of the message read alike
                        expected: if sensitive {
                            format!("`{}`", REDACTED)
                        } else {
                            describe_header_value(expected_value)
                        },
                        actual: if sensitive {
                            REDACTED.to_string()
                        } else {
                            truncate_for_display(actual_value)
                        },
                    });
                }
            }
            None => {
                debug!("Required header '{}' is missing", name);
                return Some(RejectReason::HeaderMissing {
                    name: normalized_name,
                });
            }
        }
    }

    // Check forbidden headers are not present
    for forbidden_name in &matcher.forbidden {
        let normalized_name = forbidden_name.to_lowercase();

        if request_headers.contains_key(&normalized_name) {
            debug!("Forbidden header '{}' is present", forbidden_name);
            return Some(RejectReason::HeaderForbidden {
                name: normalized_name,
            });
        }
    }

    // If strict mode, check for extra headers (excluding standard ones)
    if matcher.strict {
        for name in sorted_keys(request_headers) {
            if is_strict_ignored_header(name) {
                continue;
            }

            let is_required = matcher
                .required
                .keys()
                .any(|req| req.to_lowercase() == *name);

            if !is_required {
                debug!("Strict mode: extra header '{}' found", name);
                return Some(RejectReason::HeaderExtra { name: name.clone() });
            }
        }
    }

    None
}

/// Match a single header value
fn match_header_value(actual: &str, expected: &HeaderValue) -> bool {
    match expected {
        HeaderValue::Exact(value) => actual == value,
        HeaderValue::Pattern(pattern) => match pattern {
            HeaderPattern::Regex(regex_str) => match compiled_regex(regex_str) {
                Some(re) => re.is_match(actual),
                None => false,
            },
            HeaderPattern::Any => true,
            HeaderPattern::Prefix(prefix) => actual.starts_with(prefix),
            HeaderPattern::Contains(substring) => actual.contains(substring),
        },
    }
}

// ============================================================================
// Body Parsing and Matching
// ============================================================================

/// Parsed body content
#[derive(Debug, Clone)]
pub enum ParsedBody {
    Json(serde_json::Value),
    Text(String),
    Form(HashMap<String, String>),
    #[allow(dead_code)]
    Binary(Vec<u8>),
    Empty,
}

/// The media type of a `Content-Type` value: parameters dropped, trimmed,
/// lowercased. Media types are case-insensitive (RFC 9110 §8.3.1), so
/// `Application/JSON; charset=UTF-8` and `application/json` are the same type.
pub(crate) fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// True for `application/json` and any structured-syntax `+json` type
/// (`application/vnd.api+json`, `application/problem+json`, `application/ld+json`).
pub(crate) fn is_json_media_type(content_type: &str) -> bool {
    let media = media_type(content_type);
    media == "application/json" || media.ends_with("+json")
}

/// Parse request body based on content type
pub fn parse_body(bytes: &Bytes, content_type: Option<&str>) -> ParsedBody {
    if bytes.is_empty() {
        return ParsedBody::Empty;
    }

    match content_type {
        Some(ct) if is_json_media_type(ct) => {
            match serde_json::from_slice(bytes) {
                Ok(json) => ParsedBody::Json(json),
                Err(e) => {
                    warn!("Failed to parse JSON body: {}", e);
                    // Fall back to text
                    ParsedBody::Text(String::from_utf8_lossy(bytes).to_string())
                }
            }
        }

        Some(ct) if media_type(ct) == "application/x-www-form-urlencoded" => {
            let text = String::from_utf8_lossy(bytes);
            let form = parse_form_data(&text);
            ParsedBody::Form(form)
        }

        Some(ct) if media_type(ct).starts_with("text/") => {
            ParsedBody::Text(String::from_utf8_lossy(bytes).to_string())
        }

        _ => {
            // Try to parse as text, fall back to binary
            match String::from_utf8(bytes.to_vec()) {
                Ok(text) => ParsedBody::Text(text),
                Err(_) => ParsedBody::Binary(bytes.to_vec()),
            }
        }
    }
}

/// Parse form data (application/x-www-form-urlencoded)
///
/// Keys and values are decoded the same way query parameters are — see
/// [`decode_component`] — so `password=secret+pass` and
/// `password=secret%20pass` both produce `secret pass`, and a mock's
/// `"password": "secret pass"` matches either.
///
/// A field sent more than once (`opt=a&opt=b`) keeps its *first* value, the
/// same treatment [`parse_query_string`] gives a repeated query key (#108) —
/// consistent rather than silently keeping whichever happened to be inserted
/// last.
fn parse_form_data(text: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();

    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }

        if let Some((key, value)) = pair.split_once('=') {
            fields
                .entry(decode_component(key))
                .or_insert_with(|| decode_component(value));
        }
    }

    fields
}

/// Check if request body matches the mock's requirements
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
pub fn match_body(parsed_body: &ParsedBody, matcher: &BodyMatcher) -> bool {
    body_reject_reason(parsed_body, matcher).is_none()
}

/// Why `matcher` rejects `parsed_body`, or `None` if it accepts it.
///
/// This is the body matcher; [`match_body`] is the boolean view of it.
///
/// Reasons name the *shape* of the mismatch — which JSON field, which form
/// field, which configured substring — and never quote the request body back.
/// The body is already stored on the record verbatim; repeating it inside an
/// explanation only widens the surface a secret can appear on.
pub fn body_reject_reason(parsed_body: &ParsedBody, matcher: &BodyMatcher) -> Option<RejectReason> {
    match matcher {
        BodyMatcher::Any => None,

        BodyMatcher::Empty => {
            if matches!(parsed_body, ParsedBody::Empty) {
                None
            } else {
                Some(RejectReason::BodyNotEmpty)
            }
        }

        BodyMatcher::Json(json_matcher) => {
            if let ParsedBody::Json(actual) = parsed_body {
                json_body_reject_reason(actual, json_matcher)
            } else if let ParsedBody::Text(text) = parsed_body {
                // Try to parse text as JSON
                if let Ok(actual) = serde_json::from_str(text) {
                    json_body_reject_reason(&actual, json_matcher)
                } else {
                    debug!("Body is not valid JSON");
                    Some(RejectReason::BodyWrongKind { expected: "JSON" })
                }
            } else {
                debug!("Expected JSON body, got {:?}", parsed_body);
                Some(RejectReason::BodyWrongKind { expected: "JSON" })
            }
        }

        BodyMatcher::Text(text_matcher) => {
            if let ParsedBody::Text(actual) = parsed_body {
                text_body_reject_reason(actual, text_matcher)
            } else if let ParsedBody::Json(json) = parsed_body {
                // Convert JSON to string for text matching
                let text = json.to_string();
                text_body_reject_reason(&text, text_matcher)
            } else {
                debug!("Expected text body");
                Some(RejectReason::BodyWrongKind { expected: "text" })
            }
        }

        BodyMatcher::Form(form_matcher) => {
            if let ParsedBody::Form(actual) = parsed_body {
                form_body_reject_reason(actual, form_matcher)
            } else if let ParsedBody::Text(text) = parsed_body {
                // Try to parse as form data
                let form = parse_form_data(text);
                form_body_reject_reason(&form, form_matcher)
            } else {
                debug!("Expected form body");
                Some(RejectReason::BodyWrongKind {
                    expected: "form data",
                })
            }
        }
    }
}

#[allow(dead_code)] // boolean view of the matcher below; exercised by the test suite
fn match_json_body(actual: &serde_json::Value, matcher: &JsonBodyMatcher) -> bool {
    json_body_reject_reason(actual, matcher).is_none()
}

/// Why a JSON body matcher rejects `actual`, or `None` if it accepts it.
fn json_body_reject_reason(
    actual: &serde_json::Value,
    matcher: &JsonBodyMatcher,
) -> Option<RejectReason> {
    // Exact match
    if let Some(ref expected) = matcher.exact {
        if actual != expected {
            debug!("JSON exact match failed");
            return Some(RejectReason::JsonExact);
        }
    }

    // Partial match
    if let Some(ref expected) = matcher.partial {
        if let Some(reason) = json_partial_reject_reason(actual, expected, matcher.strict, "") {
            debug!("JSON partial match failed");
            return Some(reason);
        }
    }

    None
}

/// Partial JSON match: check if actual contains all fields from expected
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
fn match_json_partial(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    strict: bool,
) -> bool {
    json_partial_reject_reason(actual, expected, strict, "").is_none()
}

/// Why a partial JSON match rejects `actual`, or `None` if it accepts it.
///
/// `field_path` is the dotted path walked so far (empty at the root), so a
/// nested mismatch can name `user.address.city` rather than just "some field".
fn json_partial_reject_reason(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    strict: bool,
    field_path: &str,
) -> Option<RejectReason> {
    /// Extend a dotted field path with one more segment.
    fn child(field_path: &str, key: &str) -> String {
        if field_path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{}", field_path, key)
        }
    }

    match (actual, expected) {
        (serde_json::Value::Object(actual_obj), serde_json::Value::Object(expected_obj)) => {
            // Check all expected fields exist and match
            for (key, expected_value) in expected_obj {
                match actual_obj.get(key) {
                    Some(actual_value) => {
                        if let Some(reason) = json_partial_reject_reason(
                            actual_value,
                            expected_value,
                            strict,
                            &child(field_path, key),
                        ) {
                            return Some(reason);
                        }
                    }
                    None => {
                        debug!("Expected field '{}' missing in actual", key);
                        return Some(RejectReason::JsonFieldMissing {
                            field: child(field_path, key),
                        });
                    }
                }
            }

            // If strict, check for extra fields
            if strict {
                for key in actual_obj.keys() {
                    if !expected_obj.contains_key(key) {
                        debug!("Strict mode: extra field '{}' in actual", key);
                        return Some(RejectReason::JsonFieldExtra {
                            field: child(field_path, key),
                        });
                    }
                }
            }

            None
        }

        (serde_json::Value::Array(actual_arr), serde_json::Value::Array(expected_arr)) => {
            if strict && actual_arr.len() != expected_arr.len() {
                return Some(RejectReason::JsonArrayLength {
                    field: describe_field_path(field_path),
                    expected: expected_arr.len(),
                    actual: actual_arr.len(),
                });
            }

            // Check if all expected items match in order
            for (i, expected_item) in expected_arr.iter().enumerate() {
                if let Some(actual_item) = actual_arr.get(i) {
                    if let Some(reason) = json_partial_reject_reason(
                        actual_item,
                        expected_item,
                        strict,
                        &format!("{}[{}]", field_path, i),
                    ) {
                        return Some(reason);
                    }
                } else {
                    return Some(RejectReason::JsonFieldMissing {
                        field: format!("{}[{}]", field_path, i),
                    });
                }
            }

            None
        }

        _ => {
            if actual == expected {
                None
            } else {
                Some(RejectReason::JsonFieldMismatch {
                    field: describe_field_path(field_path),
                })
            }
        }
    }
}

/// Match text body
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
fn match_text_body(actual: &str, matcher: &TextBodyMatcher) -> bool {
    text_body_reject_reason(actual, matcher).is_none()
}

/// Why a text body matcher rejects `actual`, or `None` if it accepts it.
fn text_body_reject_reason(actual: &str, matcher: &TextBodyMatcher) -> Option<RejectReason> {
    // Exact match
    if let Some(ref expected) = matcher.exact {
        if actual != expected {
            debug!("Text exact match failed");
            return Some(RejectReason::TextExact);
        }
    }

    // Contains
    if let Some(ref substring) = matcher.contains {
        if !actual.contains(substring) {
            debug!("Text does not contain '{}'", substring);
            return Some(RejectReason::TextContains {
                substring: truncate_for_display(substring),
            });
        }
    }

    // Regex
    if let Some(ref pattern) = matcher.regex {
        match compiled_regex(pattern) {
            Some(re) => {
                if !re.is_match(actual) {
                    debug!("Text does not match regex '{}'", pattern);
                    return Some(RejectReason::TextRegex {
                        pattern: truncate_for_display(pattern),
                    });
                }
            }
            None => {
                return Some(RejectReason::InvalidRegex {
                    pattern: truncate_for_display(pattern),
                })
            }
        }
    }

    None
}

/// Match form body
#[allow(dead_code)] // boolean view of the matcher above; exercised by the test suite
fn match_form_body(actual: &HashMap<String, String>, matcher: &FormBodyMatcher) -> bool {
    form_body_reject_reason(actual, matcher).is_none()
}

/// Why a form body matcher rejects `actual`, or `None` if it accepts it.
fn form_body_reject_reason(
    actual: &HashMap<String, String>,
    matcher: &FormBodyMatcher,
) -> Option<RejectReason> {
    // Check all required fields match, in name order for a stable report
    for (key, expected_value) in sorted_by_key(&matcher.fields) {
        match actual.get(key) {
            Some(actual_value) => {
                if actual_value != expected_value {
                    debug!(
                        "Form field '{}' mismatch: expected '{}', got '{}'",
                        key, expected_value, actual_value
                    );
                    return Some(RejectReason::FormFieldMismatch { field: key.clone() });
                }
            }
            None => {
                debug!("Required form field '{}' is missing", key);
                return Some(RejectReason::FormFieldMissing { field: key.clone() });
            }
        }
    }

    // If strict, check for extra fields
    if matcher.strict {
        for key in sorted_keys(actual) {
            if !matcher.fields.contains_key(key) {
                debug!("Strict mode: extra form field '{}' found", key);
                return Some(RejectReason::FormFieldExtra { field: key.clone() });
            }
        }
    }

    None
}

// ============================================================================
// Path Parameter Matching
// ============================================================================

/// A path template compiled into a regex, plus the parameter names in the
/// order their capture groups appear. Supports `:name` (Express-style) and
/// `{name}` (OpenAPI-style) segments.
#[derive(Debug, Clone)]
pub struct CompiledPathPattern {
    pub regex: Regex,
    pub param_names: Vec<String>,
    /// How many `/` the template contains — and so any path it can match.
    slashes: usize,
}

/// True if any path segment uses `:name` or `{name}` parameter syntax.
pub fn is_pattern_path(path: &str) -> bool {
    // Every parameter syntax contains `:` or `{`; most routes contain neither,
    // and this runs once per registered route per unmatched-exact request.
    if !path.contains([':', '{']) {
        return false;
    }
    path.split('/').any(|segment| {
        (segment.starts_with(':') && segment.len() > 1)
            || (segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2)
    })
}

/// Compile (or reuse) the pattern for `path`.
///
/// Path templates come from mock files, so the same handful of strings are
/// matched against on every request. Compiling them is memoized process-wide:
/// a given template costs one `Regex::new` for the lifetime of the process,
/// not one per request. Hot reload swapping the mock map doesn't invalidate
/// anything — the cache is keyed by the template string itself.
pub fn compile_path_pattern(path: &str) -> Option<Arc<CompiledPathPattern>> {
    cached(&PATH_PATTERN_CACHE, path, build_path_pattern)
}

static PATH_PATTERN_CACHE: CompileCache<CompiledPathPattern> = OnceLock::new();

/// True if `path` has ever been compiled in this process. Test-only probe for
/// asserting that a request never reached the path-pattern compiler.
#[cfg(test)]
pub fn path_pattern_is_cached(path: &str) -> bool {
    crate::regex_cache::is_cached(&PATH_PATTERN_CACHE, path)
}

/// Compile a path template into a regex with one (unnamed, positional)
/// capture group per parameter segment — using positional groups rather
/// than Rust regex's named-group syntax means parameter names aren't
/// restricted to `[A-Za-z0-9_]` (e.g. `:org-id` works).
///
/// Returns `None` if the path has no parameter segments, or the resulting
/// regex fails to compile.
fn build_path_pattern(path: &str) -> Option<CompiledPathPattern> {
    let mut param_names = Vec::new();
    let mut regex_parts = Vec::new();

    for segment in path.split('/') {
        if let Some(name) = segment.strip_prefix(':').filter(|n| !n.is_empty()) {
            param_names.push(name.to_string());
            regex_parts.push("([^/]+)".to_string());
        } else if segment.len() > 2 && segment.starts_with('{') && segment.ends_with('}') {
            param_names.push(segment[1..segment.len() - 1].to_string());
            regex_parts.push("([^/]+)".to_string());
        } else {
            regex_parts.push(regex::escape(segment));
        }
    }

    if param_names.is_empty() {
        return None;
    }

    let pattern = format!("^{}$", regex_parts.join("/"));
    match Regex::new(&pattern) {
        Ok(regex) => Some(CompiledPathPattern {
            regex,
            param_names,
            slashes: path.bytes().filter(|&b| b == b'/').count(),
        }),
        Err(e) => {
            warn!("Invalid path parameter pattern '{}': {}", path, e);
            None
        }
    }
}

/// A cheap necessary condition for `template` (a declared pattern path) to
/// match `request_path`: the same number of segments, and the same first segment
/// whenever the template's first segment is a literal.
///
/// A request is checked against every pattern route registered for its method,
/// so this runs once per route; rejecting the great majority here keeps the cache
/// lookup and the regex off that loop. It never rejects a path the template
/// would match.
fn template_may_match(template: &str, request_path: &str) -> bool {
    let slashes = |s: &str| s.bytes().filter(|&b| b == b'/').count();
    if slashes(template) != slashes(request_path) {
        return false;
    }
    let template_first = template
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    let is_param = |seg: &str| {
        (seg.starts_with(':') && seg.len() > 1)
            || (seg.starts_with('{') && seg.ends_with('}') && seg.len() > 2)
    };
    if is_param(template_first) {
        return true;
    }
    request_path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        == template_first
}

/// Match a concrete request path against a compiled pattern, returning the
/// captured parameter values keyed by name, or `None` if it doesn't match.
pub fn match_path_pattern(
    pattern: &CompiledPathPattern,
    request_path: &str,
) -> Option<HashMap<String, String>> {
    // A template matches only paths with the same number of segments, so the
    // vast majority of non-matching routes are rejected by counting slashes —
    // cheaper than running the regex, and a request is checked against every
    // pattern route registered for its method.
    if request_path.bytes().filter(|&b| b == b'/').count() != pattern.slashes {
        return None;
    }
    let caps = pattern.regex.captures(request_path)?;
    let mut params = HashMap::with_capacity(pattern.param_names.len());
    for (i, name) in pattern.param_names.iter().enumerate() {
        let value = caps.get(i + 1)?.as_str().to_string();
        params.insert(name.clone(), value);
    }
    Some(params)
}

// ============================================================================
// Trailing-slash tolerance (#119)
// ============================================================================

/// Environment variable restoring pre-#119 behavior: `/things` and `/things/`
/// treated as entirely distinct paths, with no fallback tried between them.
pub const STRICT_TRAILING_SLASH_ENV: &str = "MIMIC_STRICT_TRAILING_SLASH";

/// Whether trailing-slash tolerance is switched off, read from
/// [`STRICT_TRAILING_SLASH_ENV`]. Off (i.e. tolerant) unless set truthy —
/// matching behavior before this setting existed.
fn strict_trailing_slash() -> bool {
    std::env::var(STRICT_TRAILING_SLASH_ENV)
        .map(|v| crate::types::is_truthy(&v))
        .unwrap_or(false)
}

/// The other trailing-slash form of `path` — the slash added if it's
/// missing, removed if it's there — or `None` when there's nothing to try:
/// the root path (which trailing-slash tolerance leaves alone) has no other
/// form.
fn trailing_slash_variant(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    match path.strip_suffix('/') {
        Some(stripped) => Some(stripped.to_string()),
        None => Some(format!("{}/", path)),
    }
}

// ============================================================================
// HEAD from GET fallback (#112)
// ============================================================================

/// Environment variable switching the HEAD-from-GET fallback off, restoring
/// the pre-#112 behavior: a `HEAD` request only ever answered by an explicit
/// `HEAD` mock, 404 otherwise.
pub const HEAD_FROM_GET_ENV: &str = "MIMIC_HEAD_FROM_GET";

/// Whether an unmatched `HEAD` request may fall back to the mock `GET` would
/// have matched, read from [`HEAD_FROM_GET_ENV`]. **On by default** — the
/// fallback is the RFC 9110 answer, and the setting exists to switch it off —
/// so unlike [`strict_trailing_slash`] the polarity is inverted: a set-but-
/// falsy value (`false`, `0`, `no`, `off`) disables the fallback, and unset
/// or anything truthy keeps it.
fn head_from_get_enabled() -> bool {
    std::env::var(HEAD_FROM_GET_ENV)
        .map(|v| crate::types::is_truthy(&v))
        .unwrap_or(true)
}

// ============================================================================
// Mock Finding - Find best matching mock
// ============================================================================

/// Score deducted from a pattern (path-parameter) match relative to an exact
/// path match, so an exact path always outranks a pattern when both match.
const PATTERN_MATCH_PENALTY: u32 = 100;

/// Result of mock matching with score for ranking
#[derive(Debug)]
pub struct MatchResult {
    pub mock: MockConfig,
    pub score: u32,
    /// Position of the mock within its METHOD:PATH bucket, used to key
    /// per-mock sequence counters when several mocks share a path
    pub index: usize,
    /// Values captured from named path parameters; empty for exact matches
    pub path_params: HashMap<String, String>,
    /// The "METHOD:path" key the matched mock is registered under — the
    /// mock's declared path (which may be a pattern like `/users/:id`),
    /// not necessarily the concrete request path
    pub matched_key: String,
    /// The components `score` was assembled from, so the win can be explained
    /// without re-running the matchers
    pub breakdown: ScoreBreakdown,
    /// True when this mock was only reached by trying the trailing-slash
    /// toggled form of the request path — the literal path (as an exact key
    /// or against every pattern route) matched nothing at all. See
    /// [`STRICT_TRAILING_SLASH_ENV`].
    pub trailing_slash_normalized: bool,
    /// True when this mock was only reached by retrying a `HEAD` request
    /// against the mock `GET` would have matched — no `HEAD` mock matched.
    /// The handler strips the response body and the explanation notes the
    /// rewrite. See [`HEAD_FROM_GET_ENV`].
    pub head_from_get: bool,
}

/// Count the literal — i.e. non-`:name`, non-`{name}` — segments of a declared
/// mock path. `/users/:id` scores 1, `/{resource}/:id` scores 0, and an exact
/// path scores one per segment.
fn path_specificity(path: &str) -> u32 {
    path.split('/')
        .filter(|segment| {
            let is_param = (segment.starts_with(':') && segment.len() > 1)
                || (segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2);
            !is_param && !segment.is_empty()
        })
        .count() as u32
}

/// Decide whether the request body must be read, knowing only the method and
/// path — this runs *before* the body is consumed, so the eventual matched
/// mock isn't known yet.
///
/// The body is read when either:
///
/// - some mock registered for this method+path asks for it, i.e. it declares a
///   `body` matcher, sets `consume_body: true`, or interpolates `{{body.…}}`
///   into its response; or
/// - no mock is registered for this method+path at all, so the request is
///   heading for a 404 and the body is worth capturing for the request log.
///
/// Otherwise the body is left unread, which is what `consume_body: false`
/// (the default) promises.
///
/// `active_tags` is the scenario selection (see [`MockConfig::is_active`]);
/// mocks filtered out by it are invisible here for the same reason they're
/// invisible to [`find_matching_mock`] — a request only tag-filtered mocks
/// could serve is heading for a 404, and its body is worth logging.
pub fn requires_body(
    method: &str,
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> bool {
    /// A single mock's own answer to "do I need the request body?"
    fn mock_needs_body(mock: &MockConfig) -> bool {
        mock.consume_body
            || mock.body.is_some()
            || crate::template::references_body(&mock.response)
            || mock.sequence.as_deref().is_some_and(|steps| {
                steps
                    .iter()
                    .any(|step| crate::template::references_body(&step.response))
            })
            // A templated `response_file` reads `{{body.…}}` out of a file
            // rather than out of the mock JSON, and needs the body just the
            // same.
            || mock.file_body_references_request_body()
    }

    let scan = scan_candidates(
        method,
        path,
        mocks,
        active_tags,
        strict_trailing_slash(),
        mock_needs_body,
    );
    scan.probe_hit || !scan.any_candidate
}

/// True if any *active* mock is registered for `method` + `path`, whether under
/// an exact key or a pattern route.
///
/// Deliberately weaker than [`find_matching_mock`]: it asks whether an endpoint
/// exists, not whether a given request would match one. That's the question a
/// CORS preflight poses — it carries none of the real request's body or
/// headers, so running the full matchers against it would 404 every preflight
/// for an endpoint that happens to declare a `body` or `headers` matcher.
///
/// A `HEAD` with no mock of its own looks at what `GET` would have had — the
/// same rewrite [`find_matching_mock`] applies, so a preflight for
/// `Access-Control-Request-Method: HEAD` is answered exactly when the actual
/// `HEAD` would be. See [`HEAD_FROM_GET_ENV`].
pub fn route_exists(
    method: &str,
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> bool {
    let exists = scan_candidates(
        method,
        path,
        mocks,
        active_tags,
        strict_trailing_slash(),
        |_| true,
    )
    .any_candidate;

    if exists || !head_from_get_enabled() || !method.eq_ignore_ascii_case("HEAD") {
        return exists;
    }

    scan_candidates(
        "GET",
        path,
        mocks,
        active_tags,
        strict_trailing_slash(),
        |_| true,
    )
    .any_candidate
}

/// What [`scan_candidates`] found.
struct CandidateScan {
    /// Some active mock is registered for the method + path.
    any_candidate: bool,
    /// `probe` accepted one of them, and the scan stopped there.
    probe_hit: bool,
}

/// Visit every active mock registered for `method` that could serve `path`,
/// stopping as soon as `probe` accepts one. Falls back to `path`'s
/// trailing-slash variant when the literal path has no candidate at all and
/// `strict` doesn't forbid it (see [`STRICT_TRAILING_SLASH_ENV`]) — exactly
/// the fallback [`find_matching_mock`] applies, so "is there an endpoint
/// behind this preflight?" and "would a request here need its body?" agree
/// with what a real request to `path` would actually match.
///
/// "Does the body need reading?" and "is there an endpoint behind this
/// preflight?" are two questions about the same candidate set, both asked
/// before any matcher has run — so they share one exact-key-then-pattern-scan
/// rather than two copies of it that could drift apart.
fn scan_candidates(
    method: &str,
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
    strict: bool,
    mut probe: impl FnMut(&MockConfig) -> bool,
) -> CandidateScan {
    let scan = scan_candidates_at(method, path, mocks, active_tags, &mut probe);
    if scan.any_candidate || strict {
        return scan;
    }
    match trailing_slash_variant(path) {
        Some(variant) => scan_candidates_at(method, &variant, mocks, active_tags, &mut probe),
        None => scan,
    }
}

/// [`scan_candidates`]'s scan against one literal path, with no trailing-slash
/// fallback of its own.
fn scan_candidates_at(
    method: &str,
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
    probe: &mut dyn FnMut(&MockConfig) -> bool,
) -> CandidateScan {
    let mut scan = CandidateScan {
        any_candidate: false,
        probe_hit: false,
    };

    let base_key = crate::types::create_mock_key(method, path);
    if let Some(mock_list) = mocks.get(&base_key) {
        for mock in mock_list.iter().filter(|m| m.is_active(active_tags)) {
            scan.any_candidate = true;
            if probe(mock) {
                scan.probe_hit = true;
                return scan;
            }
        }
    }

    // Pattern routes can serve this path too, either as the winner or as the
    // fallback when the exact key's matchers reject the request.
    let method_prefix = format!("{}:", method.to_uppercase());
    for (key, mock_list) in mocks.iter() {
        if *key == base_key {
            continue;
        }
        let Some(path_part) = key.strip_prefix(method_prefix.as_str()) else {
            continue;
        };
        if !is_pattern_path(path_part) {
            continue;
        }
        if !template_may_match(path_part, path) {
            continue;
        }
        let Some(pattern) = compile_path_pattern(path_part) else {
            continue;
        };
        if match_path_pattern(&pattern, path).is_none() {
            continue;
        }

        for mock in mock_list.iter().filter(|m| m.is_active(active_tags)) {
            scan.any_candidate = true;
            if probe(mock) {
                scan.probe_hit = true;
                return scan;
            }
        }
    }

    scan
}

/// Find the best matching mock for a request.
///
/// Exact "METHOD:path" lookups are O(1) and tried first. Parameterized paths
/// (`:id`, `{id}`) are only scanned when the exact lookup produced no match at
/// all, so a mock set with no path parameters — or a request that hits one
/// exactly — never touches the pattern loop. A pattern match also always
/// scores below an otherwise-equal exact match.
///
/// `active_tags` is the scenario selection: mocks it filters out (see
/// [`MockConfig::is_active`]) are skipped before scoring, so they behave
/// exactly as if they weren't loaded. `None`, and any mock without `tags`,
/// means no filtering — the zero-config behavior.
///
/// A request whose literal path matches nothing at all — no exact key, no
/// pattern route — is retried against its trailing-slash variant (`/things`
/// for `/things/`, and vice versa) unless [`STRICT_TRAILING_SLASH_ENV`] is
/// set. The literal path always gets first refusal: this fallback only ever
/// runs once it has come up completely empty, so a mock deliberately
/// registered at `/things/` keeps serving only `/things/` for as long as a
/// sibling `/things` mock also exists.
///
/// A `HEAD` request whose lookup comes up empty through all of the above is
/// finally retried as `GET` — RFC 9110 defines `HEAD` as `GET` without the
/// body, so the mapping is the standard's, not an invention. An explicit
/// `HEAD` mock always wins, exactly as an explicit mock beats every other
/// fallback. `MIMIC_HEAD_FROM_GET=false` restores the pre-#112 404. See
/// [`HEAD_FROM_GET_ENV`].
pub fn find_matching_mock(
    context: &RequestContext,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> Option<MatchResult> {
    let mut result = find_matching_mock_at(context, &context.path, mocks, active_tags);

    // The trailing-slash fallback only runs once the literal path has come up
    // completely empty — and regardless of whether the strict knob governs it,
    // the HEAD-from-GET fallback below stays reachable, the two settings being
    // orthogonal.
    if result.is_none() && !strict_trailing_slash() {
        if let Some(variant) = trailing_slash_variant(&context.path) {
            if let Some(mut fallback) = find_matching_mock_at(context, &variant, mocks, active_tags)
            {
                fallback.trailing_slash_normalized = true;
                result = Some(fallback);
            }
        }
    }

    if let Some(result) = result {
        return Some(result);
    }

    // Only a `HEAD` that nothing matched is rewritten as `GET`: an explicit
    // `HEAD` mock always wins because it would have matched above. The retry
    // is a full `find_matching_mock` call so the rewrite composes with the
    // trailing-slash fallback — `HEAD /users/` can still reach `GET:/users`.
    // `context`'s request data (query params, headers, body) is untouched, so
    // every GET-mock matcher judges the HEAD request as-is.
    if !head_from_get_enabled() || !context.method.eq_ignore_ascii_case("HEAD") {
        return None;
    }

    let mut get_context = context.clone();
    get_context.method = "GET".to_string();
    let mut result = find_matching_mock(&get_context, mocks, active_tags)?;
    result.head_from_get = true;
    Some(result)
}

/// [`find_matching_mock`]'s search against one literal `lookup_path` — either
/// the request's own path, or its trailing-slash variant. `context` still
/// carries the request's real path and is what every matcher (query params,
/// headers, body) scores against; only the exact-key lookup and the pattern
/// scan use `lookup_path`.
fn find_matching_mock_at(
    context: &RequestContext,
    lookup_path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> Option<MatchResult> {
    let base_key = crate::types::create_mock_key(&context.method, lookup_path);

    debug!(
        "Looking for mock: {} (key: {})",
        format!("{} {}", context.method, lookup_path),
        base_key
    );

    // Candidates borrow their mock. A deep copy of a mock (its whole response
    // body) used to be made for every candidate that matched, to be thrown away
    // for every one but the winner; only the winner is cloned now.
    struct Candidate<'a> {
        mock: &'a MockConfig,
        score: u32,
        index: usize,
        path_params: HashMap<String, String>,
        matched_key: String,
        specificity: u32,
        breakdown: ScoreBreakdown,
    }
    let mut candidates: Vec<Candidate> = Vec::new();

    // Exact match: O(1) lookup, no path params, full score.
    if let Some(mock_list) = mocks.get(&base_key) {
        for (index, mock) in mock_list.iter().enumerate() {
            // Note the index comes from the unfiltered list: it keys sequence
            // counters and hit counts, so it must not shift when a scenario
            // hides a sibling mock registered under the same path.
            if !mock.is_active(active_tags) {
                continue;
            }
            if let Some(breakdown) = calculate_match_score(context, mock) {
                candidates.push(Candidate {
                    mock,
                    score: breakdown.total(),
                    index,
                    path_params: HashMap::new(),
                    matched_key: base_key.clone(),
                    specificity: path_specificity(lookup_path),
                    breakdown,
                });
            }
        }
    }

    // Pattern match: only reached when nothing matched the exact key. Scans
    // mocks registered under the same method whose path uses :name/{name}
    // segments; each template's regex is compiled once process-wide, not
    // once per request.
    if candidates.is_empty() {
        let method_prefix = format!("{}:", context.method.to_uppercase());
        for (key, mock_list) in mocks.iter() {
            if *key == base_key {
                continue; // already handled as an exact match above
            }
            let Some(path_part) = key.strip_prefix(method_prefix.as_str()) else {
                continue;
            };
            if !is_pattern_path(path_part) {
                continue;
            }
            if !template_may_match(path_part, lookup_path) {
                continue;
            }
            let Some(pattern) = compile_path_pattern(path_part) else {
                continue;
            };
            let Some(params) = match_path_pattern(&pattern, lookup_path) else {
                continue;
            };

            for (index, mock) in mock_list.iter().enumerate() {
                if !mock.is_active(active_tags) {
                    continue;
                }
                if let Some(breakdown) = calculate_match_score(context, mock) {
                    candidates.push(Candidate {
                        mock,
                        score: breakdown.total().saturating_sub(PATTERN_MATCH_PENALTY),
                        index,
                        path_params: params.clone(),
                        matched_key: key.clone(),
                        specificity: path_specificity(path_part),
                        breakdown,
                    });
                }
            }
        }
    }

    // Rank candidates. Score decides first; the remaining keys exist so the
    // winner is a pure function of the mock set, never of `HashMap` iteration
    // order — which is reseeded on every hot reload and would otherwise let
    // two equally-scored pattern mocks trade places every couple of seconds.
    //
    //   1. highest score
    //   2. most literal path segments (`/users/:id` beats `/{any}/:id`)
    //   3. lowest registered key, lexicographically
    //   4. lowest position within that key's bucket
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.specificity.cmp(&a.specificity))
            .then_with(|| a.matched_key.cmp(&b.matched_key))
            .then_with(|| a.index.cmp(&b.index))
    });

    if let Some(best) = candidates.into_iter().next() {
        debug!("Found matching mock with score {}", best.score);
        Some(MatchResult {
            mock: best.mock.clone(),
            score: best.score,
            index: best.index,
            path_params: best.path_params,
            matched_key: best.matched_key,
            breakdown: best.breakdown,
            trailing_slash_normalized: false,
            head_from_get: false,
        })
    } else {
        debug!("No matching mock found");
        None
    }
}

/// Score awarded for a method + path match, before any matcher bonuses.
const BASE_SCORE: u32 = 1000;
/// Score added per declared query parameter.
const QUERY_PARAM_SCORE: u32 = 100;
/// Score added per declared required header.
const HEADER_SCORE: u32 = 50;
/// Score added for a satisfied body matcher.
const BODY_SCORE: u32 = 500;

/// How a mock's score was arrived at, kept so the dashboard can show the
/// arithmetic instead of an unexplained number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScoreBreakdown {
    pub base: u32,
    pub query_params: u32,
    pub headers: u32,
    pub body: u32,
}

impl ScoreBreakdown {
    fn total(&self) -> u32 {
        self.base + self.query_params + self.headers + self.body
    }
}

/// The outcome of running every one of a mock's matchers against a request.
#[derive(Debug, Clone)]
pub enum Evaluation {
    Matched(ScoreBreakdown),
    Rejected(RejectReason),
}

/// Run a mock's matchers against a request: the single place matching is
/// decided.
///
/// [`calculate_match_score`] (does it match, and how well?) and
/// [`explain_no_match`] (why didn't it?) are both views of this one function,
/// which is what keeps the dashboard's explanations honest.
pub fn evaluate_mock(context: &RequestContext, mock: &MockConfig) -> Evaluation {
    let mut breakdown = ScoreBreakdown {
        base: BASE_SCORE, // Base score for method + path match
        ..Default::default()
    };

    // Check query params
    if let Some(ref query_matcher) = mock.query_params {
        if let Some(reason) = query_params_reject_reason_all(
            &context.query_params,
            Some(&context.query_params_all),
            query_matcher,
        ) {
            return Evaluation::Rejected(reason);
        }
        // Add score based on number of matched params
        breakdown.query_params = (query_matcher.params.len() as u32) * QUERY_PARAM_SCORE;
    }

    // Check headers
    if let Some(ref header_matcher) = mock.headers {
        if let Some(reason) = headers_reject_reason(&context.headers, header_matcher) {
            return Evaluation::Rejected(reason);
        }
        // Add score based on number of matched headers
        breakdown.headers = (header_matcher.required.len() as u32) * HEADER_SCORE;
    }

    // Check body
    if let Some(ref body_matcher) = mock.body {
        if let Some(ref body_bytes) = context.body {
            let parsed_body = parse_body(body_bytes, context.content_type.as_deref());
            if let Some(reason) = body_reject_reason(&parsed_body, body_matcher) {
                return Evaluation::Rejected(reason);
            }
            // Body matching is worth the most
            breakdown.body = BODY_SCORE;
        } else if !matches!(body_matcher, BodyMatcher::Empty | BodyMatcher::Any) {
            // Body required but not present
            return Evaluation::Rejected(RejectReason::BodyAbsent);
        }
    }

    Evaluation::Matched(breakdown)
}

/// Calculate match score for a mock
/// Returns None if the mock doesn't match, Some(score) if it matches
fn calculate_match_score(context: &RequestContext, mock: &MockConfig) -> Option<ScoreBreakdown> {
    match evaluate_mock(context, mock) {
        Evaluation::Matched(breakdown) => Some(breakdown),
        Evaluation::Rejected(_) => None,
    }
}

// ============================================================================
// Match Explanations - why this mock, or why none
// ============================================================================

/// How a mock is named in an explanation: its source file when it was loaded
/// from one, otherwise its registration key and position.
fn describe_mock(mock: &MockConfig, key: &str, index: usize) -> String {
    match mock.source {
        Some(ref source) => source.clone(),
        None => format!("`{}` #{}", key, index),
    }
}

/// Explain, in one line, why the winning mock won.
///
/// Reads only the [`MatchResult`], whose breakdown was recorded when the score
/// was computed — nothing is re-derived here, so the arithmetic shown is
/// always the arithmetic that ran.
pub fn explain_match(result: &MatchResult) -> String {
    let mut parts = vec![format!("method+path {}", result.breakdown.base)];
    if result.breakdown.query_params > 0 {
        parts.push(format!("query params +{}", result.breakdown.query_params));
    }
    if result.breakdown.headers > 0 {
        parts.push(format!("headers +{}", result.breakdown.headers));
    }
    if result.breakdown.body > 0 {
        parts.push(format!("body +{}", result.breakdown.body));
    }
    // A pattern route always captures at least one parameter, so a non-empty
    // capture map is exactly the case the penalty was applied to.
    if !result.path_params.is_empty() {
        parts.push(format!("path pattern -{}", PATTERN_MATCH_PENALTY));
    }
    if result.trailing_slash_normalized {
        parts.push("trailing slash normalized".to_string());
    }
    if result.head_from_get {
        parts.push(format!(
            "HEAD answered from the GET mock at `{}`",
            result.matched_key
        ));
    }

    format!(
        "matched {} (score {}: {})",
        describe_mock(&result.mock, &result.matched_key, result.index),
        result.score,
        parts.join(", ")
    )
}

/// Longest list of near-miss candidates spelled out in a no-match explanation.
const MAX_EXPLAINED_CANDIDATES: usize = 5;

/// Judge every exact-key or pattern-route mock registered for
/// `context.method` + `lookup_path`, sorting each into `candidates` (an
/// active mock whose matchers rejected the request) or `tag_filtered` (a
/// mock a scenario hid that would otherwise have matched) — the same
/// candidate discovery [`find_matching_mock`] does, so [`explain_no_match`]
/// judges exactly the set a real request to `lookup_path` would have.
///
/// A plain function taking the two accumulators by `&mut Vec` rather than a
/// closure over them: [`explain_no_match`] calls this twice — once for the
/// literal path, once for its trailing-slash variant — and needs to inspect
/// `candidates`/`tag_filtered` in between, which a closure holding them by
/// mutable capture for its own lifetime cannot allow.
fn accumulate_candidates_for(
    context: &RequestContext,
    lookup_path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
    candidates: &mut Vec<(String, RejectReason)>,
    tag_filtered: &mut Vec<String>,
) {
    let mut collect = |key: &str, mock_list: &[MockConfig]| {
        for (index, mock) in mock_list.iter().enumerate() {
            if !mock.is_active(active_tags) {
                // Would this mock have matched if its tags were active? Only
                // then is it worth mentioning; a tagged mock whose matchers
                // reject the request is just a miss like any other.
                if matches!(evaluate_mock(context, mock), Evaluation::Matched(_)) {
                    tag_filtered.push(describe_mock(mock, key, index));
                }
                continue;
            }
            if let Evaluation::Rejected(reason) = evaluate_mock(context, mock) {
                candidates.push((describe_mock(mock, key, index), reason));
            }
        }
    };

    let key = crate::types::create_mock_key(&context.method, lookup_path);
    if let Some(mock_list) = mocks.get(&key) {
        collect(&key, mock_list);
    }

    // Pattern routes are candidates for this path too — sorted so the report
    // doesn't reshuffle with `HashMap` iteration order.
    let method_prefix = format!("{}:", context.method.to_uppercase());
    for candidate_key in sorted_keys(mocks) {
        if *candidate_key == key {
            continue;
        }
        let Some(path_part) = candidate_key.strip_prefix(method_prefix.as_str()) else {
            continue;
        };
        if !is_pattern_path(path_part) {
            continue;
        }
        if !template_may_match(path_part, lookup_path) {
            continue;
        }
        let Some(pattern) = compile_path_pattern(path_part) else {
            continue;
        };
        if match_path_pattern(&pattern, lookup_path).is_none() {
            continue;
        }
        if let Some(mock_list) = mocks.get(candidate_key) {
            collect(candidate_key, mock_list);
        }
    }
}

/// Explain why nothing matched: which mocks were even in the running, and the
/// first matcher that rejected each.
///
/// Candidates are gathered exactly the way [`find_matching_mock`] gathers
/// them — the mocks under the request's `METHOD:path` key, plus any pattern
/// route whose template the path fits — and each is judged by the same
/// [`evaluate_mock`] that produced the 404.
///
/// Meant to be called only on the 404 path: it walks the mock map and formats
/// strings, so the matched path never pays for it.
///
/// Mocks hidden by the scenario selection are reported as a count rather than
/// judged by their matchers — "3 mock(s) match this request but are filtered
/// out by inactive tags" is the answer somebody staring at an unexpected 404
/// actually needs. This string reaches the debug log and `/admin/requests`,
/// never the 404 body, so scenario configuration isn't leaked to API clients.
pub fn explain_no_match(
    context: &RequestContext,
    mocks: &HashMap<String, Vec<MockConfig>>,
    active_tags: Option<&HashSet<String>>,
) -> Option<String> {
    let base_key = crate::types::create_mock_key(&context.method, &context.path);
    let mut candidates: Vec<(String, RejectReason)> = Vec::new();
    let mut tag_filtered: Vec<String> = Vec::new();

    accumulate_candidates_for(
        context,
        &context.path,
        mocks,
        active_tags,
        &mut candidates,
        &mut tag_filtered,
    );

    // The literal path came up completely empty: also check its
    // trailing-slash variant, mirroring the fallback `find_matching_mock`
    // itself applies — a mock registered at `/things` that rejects a
    // `/things/` request (say, on its body matcher) should be reported as a
    // near miss, not hidden behind "no mock is registered".
    let mut trailing_slash_checked = false;
    if candidates.is_empty() && tag_filtered.is_empty() && !strict_trailing_slash() {
        if let Some(variant) = trailing_slash_variant(&context.path) {
            trailing_slash_checked = true;
            accumulate_candidates_for(
                context,
                &variant,
                mocks,
                active_tags,
                &mut candidates,
                &mut tag_filtered,
            );
        }
    }

    // The scenario hint, built once and reused by both branches below.
    let tag_note = (!tag_filtered.is_empty()).then(|| {
        let shown: Vec<String> = tag_filtered
            .iter()
            .take(MAX_EXPLAINED_CANDIDATES)
            .cloned()
            .collect();
        let mut note = format!(
            "{} mock(s) match `{}` but are filtered out by inactive tags: {}",
            tag_filtered.len(),
            base_key,
            shown.join(", ")
        );
        if tag_filtered.len() > shown.len() {
            note.push_str(&format!(" (+{} more)", tag_filtered.len() - shown.len()));
        }
        note
    });

    if candidates.is_empty() {
        // A scenario hiding the only mock that would have served this request
        // is the whole explanation — say that instead of "nothing registered".
        if let Some(note) = tag_note {
            return Some(note);
        }

        // Nothing was even registered for this method+path. The most common
        // cause by far is the right path on the wrong method, so say so when
        // that's what happened.
        let other_methods = methods_registered_for_path(&context.path, mocks);
        let mut message = if other_methods.is_empty() {
            format!("no mock is registered for `{}`", base_key)
        } else {
            format!(
                "no mock is registered for `{}` — `{}` is registered for {}",
                base_key,
                context.path,
                other_methods.join(", ")
            )
        };
        if trailing_slash_checked {
            message.push_str(
                " (its trailing-slash variant was also checked and had nothing registered either)",
            );
        }
        return Some(message);
    }

    let shown: Vec<String> = candidates
        .iter()
        .take(MAX_EXPLAINED_CANDIDATES)
        .map(|(name, reason)| format!("{} — {}", name, reason))
        .collect();

    let mut explanation = format!(
        "{} candidate mock(s) for `{}`, none matched: {}",
        candidates.len(),
        base_key,
        shown.join("; ")
    );
    if candidates.len() > shown.len() {
        explanation.push_str(&format!(" (+{} more)", candidates.len() - shown.len()));
    }
    if trailing_slash_checked {
        explanation.push_str("; found via trailing-slash normalization");
    }
    if let Some(note) = tag_note {
        explanation.push_str(&format!("; {}", note));
    }
    Some(explanation)
}

/// The methods, sorted, that some mock is registered under for `path` —
/// the "you meant POST, didn't you?" hint.
fn methods_registered_for_path(
    path: &str,
    mocks: &HashMap<String, Vec<MockConfig>>,
) -> Vec<String> {
    let mut methods: Vec<String> = mocks
        .keys()
        .filter_map(|key| {
            let (method, key_path) = key.split_once(':')?;
            (key_path == path).then(|| method.to_string())
        })
        .collect();
    methods.sort();
    methods.dedup();
    methods
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod benches;
#[cfg(test)]
mod explanation_tests;
#[cfg(test)]
mod matching_cost;
#[cfg(test)]
mod pattern_prefilter_tests;
#[cfg(test)]
mod request_parsing_tests;
#[cfg(test)]
mod scenario_tests;
#[cfg(test)]
mod tests;
