//! Proxy/passthrough mode: forward requests that match no local mock to a
//! real upstream API, optionally recording the exchange as a new mock file
//! so the next identical request is served from disk instead of the network.
//!
//! See the "Proxy / Record-and-Replay" section of the README for the
//! user-facing configuration story; this module is the implementation.

use crate::handler::is_textual_content_type;
use crate::matcher::{parse_body, ParsedBody, RequestContext};
use crate::types::{
    is_sensitive_header, is_truthy, BodyMatcher, FormBodyMatcher, HeaderMatcher, HeaderValue,
    JsonBodyMatcher, MockConfig, QueryParamMatcher, QueryParamValue, TextBodyMatcher,
};
use axum::http::{
    header::{CONTENT_ENCODING, CONTENT_TYPE},
    HeaderMap, Method, StatusCode,
};
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, info, warn};

// ============================================================================
// Configuration
// ============================================================================

pub const PROXY_UPSTREAM_ENV: &str = "MIMIC_PROXY_UPSTREAM";
pub const RECORD_UPSTREAM_ENV: &str = "MIMIC_RECORD_UPSTREAM";
pub const PROXY_TIMEOUT_MS_ENV: &str = "MIMIC_PROXY_TIMEOUT_MS";

const DEFAULT_PROXY_TIMEOUT_MS: u64 = 5000;

/// Everything the proxy path needs, resolved once at startup.
#[derive(Clone)]
pub struct ProxyConfig {
    pub upstream: String,
    pub record: bool,
    pub timeout_ms: u64,
    client: reqwest::Client,
}

/// Loopback hostnames a proxy target might resolve to.
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "0.0.0.0", "::1", "[::1]"];

/// Reject an upstream that would send Mimic's own unmatched requests right
/// back to itself — an infinite proxy loop otherwise indistinguishable from a
/// hang until the process runs out of file descriptors.
fn validate_upstream(upstream: &str, own_port: u16) -> Result<(), String> {
    let url = reqwest::Url::parse(upstream).map_err(|e| format!("invalid URL: {}", e))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!(
            "unsupported scheme '{}': only http/https are supported",
            url.scheme()
        ));
    }
    let host = url.host_str().unwrap_or("");
    let port = url.port_or_known_default().unwrap_or(0);
    if LOOPBACK_HOSTS.contains(&host) && port == own_port {
        return Err(format!(
            "upstream {}:{} resolves back to this server's own listening address",
            host, port
        ));
    }
    Ok(())
}

/// Read the proxy configuration from the environment, or `None` if
/// `MIMIC_PROXY_UPSTREAM` is unset — the default, unchanged 404 behavior.
///
/// `own_port` is the port Mimic itself listens on, used to reject a
/// self-referential upstream (see [`validate_upstream`]). An invalid or
/// self-referential upstream disables proxying entirely rather than starting
/// the server in a half-configured state.
pub fn configured_proxy_config(own_port: u16) -> Option<ProxyConfig> {
    let upstream = std::env::var(PROXY_UPSTREAM_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())?;

    if let Err(reason) = validate_upstream(&upstream, own_port) {
        warn!(
            "Ignoring {}='{}': {} — proxying disabled",
            PROXY_UPSTREAM_ENV, upstream, reason
        );
        return None;
    }

    let record = std::env::var(RECORD_UPSTREAM_ENV)
        .map(|v| is_truthy(&v))
        .unwrap_or(false);

    let timeout_ms = std::env::var(PROXY_TIMEOUT_MS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_PROXY_TIMEOUT_MS);

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            warn!(
                "Failed to build proxy HTTP client: {} — proxying disabled",
                e
            );
            return None;
        }
    };

    Some(ProxyConfig {
        upstream,
        record,
        timeout_ms,
        client,
    })
}

#[cfg(test)]
impl ProxyConfig {
    /// Build a `ProxyConfig` directly, bypassing `MIMIC_PROXY_UPSTREAM` and
    /// friends. Handler-level integration tests need a config that points at
    /// a per-test ephemeral port; going through the process-global env vars
    /// `configured_proxy_config` reads would race every other test doing the
    /// same.
    pub(crate) fn for_test(upstream: String, record: bool, timeout_ms: u64) -> Self {
        Self {
            upstream,
            record,
            timeout_ms,
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(timeout_ms))
                .build()
                .expect("client should build with a plain timeout"),
        }
    }
}

// ============================================================================
// Forwarding
// ============================================================================

/// Headers that describe the hop between client and Mimic (or Mimic and
/// upstream) rather than the resource itself; forwarding them verbatim in
/// either direction would misdescribe the new hop.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "host",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// `Content-Encoding` values the `reqwest` client transparently decodes
/// before we ever see the bytes — see the `gzip`, `deflate`, and `brotli`
/// features on the `reqwest` dependency (#106).
///
/// `reqwest` decodes the *body* but doesn't rewrite the *header* to match —
/// a known quirk, not a Mimic bug — so a name on this list is what
/// [`forward`] uses to know the header is now stale and strip it itself.
/// Anything not on this list (`zstd`, `identity`, a typo) is left exactly as
/// the upstream sent it, because the body behind it is still whatever the
/// upstream sent too.
const DECODED_CONTENT_ENCODINGS: &[&str] = &["gzip", "x-gzip", "deflate", "br"];

/// True when `value` (a raw `Content-Encoding` header value) names an
/// encoding [`forward`] already decoded for us.
fn is_transparently_decoded_encoding(value: &axum::http::HeaderValue) -> bool {
    value.to_str().is_ok_and(|v| {
        DECODED_CONTENT_ENCODINGS
            .iter()
            .any(|enc| v.trim().eq_ignore_ascii_case(enc))
    })
}

pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Debug)]
pub enum ProxyError {
    Timeout,
    Request(String),
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::Timeout => write!(f, "upstream request timed out"),
            ProxyError::Request(msg) => write!(f, "upstream request failed: {}", msg),
        }
    }
}

impl std::error::Error for ProxyError {}

/// Forward one request to `cfg.upstream`, returning the upstream's response
/// verbatim (status, non-hop-by-hop headers, body).
pub async fn forward(
    cfg: &ProxyConfig,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<ProxyResponse, ProxyError> {
    let mut url = format!("{}{}", cfg.upstream, path);
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }

    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|e| ProxyError::Request(format!("invalid method '{}': {}", method, e)))?;

    let mut builder = cfg.client.request(reqwest_method, url.as_str());
    for (name, value) in headers.iter() {
        if HOP_BY_HOP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    if !body.is_empty() {
        builder = builder.body(body);
    }

    debug!("Proxying {} {} -> {}", method, path, url);

    let response = builder.send().await.map_err(|e| {
        if e.is_timeout() {
            ProxyError::Timeout
        } else {
            ProxyError::Request(e.to_string())
        }
    })?;

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    // reqwest and axum both build on the same `http` crate version, so the
    // header map reqwest hands back is insertable into axum's `HeaderMap`
    // without a re-parse.
    let mut out_headers = HeaderMap::new();
    for (name, value) in response.headers().iter() {
        if HOP_BY_HOP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        // The body reqwest is about to hand us has already been decoded for
        // any of these — see `DECODED_CONTENT_ENCODINGS` — so the header
        // would otherwise tell a client to decode bytes that no longer need
        // it (#106). `content-length` needs no matching fix: it's already
        // hop-by-hop above, so axum computes the correct one from the actual
        // (decoded) body.
        if name == CONTENT_ENCODING && is_transparently_decoded_encoding(value) {
            continue;
        }
        out_headers.insert(name.clone(), value.clone());
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| ProxyError::Request(e.to_string()))?;

    Ok(ProxyResponse {
        status,
        headers: out_headers,
        body,
    })
}

// ============================================================================
// Recording (record-and-replay)
// ============================================================================

/// True if `content_type` describes a body worth turning into a JSON mock
/// response. Binary payloads (images, archives, protobuf, …) are forwarded
/// to the client as usual but never written to disk — there's no good text
/// representation for a `MockConfig.response` field, and silently base64'ing
/// arbitrary upstream bytes into a "mock" file is more surprising than
/// useful. See the README's proxy section for the workaround (hand-write the
/// mock, or drop `MIMIC_RECORD_UPSTREAM`).
fn is_recordable_content_type(content_type: Option<&str>) -> bool {
    match content_type {
        None => true,
        Some(ct) => is_textual_content_type(ct),
    }
}

/// Turn a request path into a filesystem-safe fragment for a recorded mock's
/// filename: `/users/42?x=1` (query already stripped by the caller) becomes
/// `users_42`.
fn sanitize_path_for_filename(path: &str) -> String {
    let cleaned: String = path
        .trim_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let mut collapsed = String::with_capacity(cleaned.len());
    let mut last_was_underscore = false;
    for c in cleaned.chars() {
        if c == '_' {
            if !last_was_underscore {
                collapsed.push(c);
            }
            last_was_underscore = true;
        } else {
            collapsed.push(c);
            last_was_underscore = false;
        }
    }
    let trimmed = collapsed.trim_matches('_');

    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A stable fingerprint of "what would have to be true of a future request
/// for this recording to answer it again": method, path, query, and the
/// headers/body that would end up in the recorded mock's matchers. Two
/// requests with the same signature produce (and should produce) the same
/// recorded file rather than racing to create two.
fn request_signature(context: &RequestContext) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    context.method.hash(&mut hasher);
    context.path.hash(&mut hasher);

    let mut query: Vec<(&String, &String)> = context.query_params.iter().collect();
    query.sort_by_key(|(k, _)| k.as_str());
    query.hash(&mut hasher);

    // The same header set `build_header_matcher` captures — so a request that
    // differs only in a header the matcher ignores hashes identically, and
    // dedupes onto the recording that would already answer it (#105).
    let mut headers: Vec<(&String, &String)> = context
        .headers
        .iter()
        .filter(|(k, _)| !is_sensitive_header(k) && is_recordable_header(k))
        .collect();
    headers.sort_by_key(|(k, _)| k.as_str());
    headers.hash(&mut hasher);

    if let Some(body) = &context.body {
        body.as_ref().hash(&mut hasher);
    }

    format!("{:016x}", hasher.finish())
}

/// Per-process bookkeeping so concurrent identical requests during recording
/// dedupe onto one file instead of racing to create several.
pub struct RecordState {
    inner: Mutex<RecordStateInner>,
}

#[derive(Default)]
struct RecordStateInner {
    /// Request signatures already recorded (or in flight) this process run.
    seen: std::collections::HashSet<String>,
    /// Next numeric suffix to use for a given `<method>_<sanitized-path>`
    /// filename prefix, seeded from what's already on disk the first time
    /// each prefix is seen.
    next_index: HashMap<String, usize>,
}

impl RecordState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RecordStateInner::default()),
        }
    }

    /// Reserve a file path for `signature` under `dir`, or `None` if this
    /// signature has already been recorded (or is currently being recorded)
    /// this process run — the caller should skip writing entirely.
    async fn reserve(&self, signature: &str, prefix: &str, dir: &Path) -> Option<PathBuf> {
        // Fast path: nothing to do if the signature is already known, or the
        // prefix's counter is already initialized.
        {
            let inner = self.inner.lock().unwrap();
            if inner.seen.contains(signature) {
                return None;
            }
            if inner.next_index.contains_key(prefix) {
                drop(inner);
                return self.reserve_initialized(signature, prefix, dir);
            }
        }

        // First time this prefix is recorded: seed the counter from whatever
        // is already on disk (e.g. a previous run), off the async runtime
        // since it's a blocking directory read.
        let scan_dir = dir.to_path_buf();
        let scan_prefix = prefix.to_string();
        let existing_max =
            tokio::task::spawn_blocking(move || max_existing_index(&scan_dir, &scan_prefix))
                .await
                .unwrap_or(0);

        {
            let mut inner = self.inner.lock().unwrap();
            inner
                .next_index
                .entry(prefix.to_string())
                .or_insert(existing_max);
        }

        self.reserve_initialized(signature, prefix, dir)
    }

    /// The synchronous half of [`Self::reserve`], once `prefix`'s counter is
    /// known to be initialized.
    fn reserve_initialized(&self, signature: &str, prefix: &str, dir: &Path) -> Option<PathBuf> {
        let mut inner = self.inner.lock().unwrap();
        if inner.seen.contains(signature) {
            return None;
        }
        inner.seen.insert(signature.to_string());
        let idx = inner.next_index.entry(prefix.to_string()).or_insert(0);
        *idx += 1;
        Some(dir.join(format!("{}_{}.json", prefix, idx)))
    }
}

impl Default for RecordState {
    fn default() -> Self {
        Self::new()
    }
}

/// The highest `<prefix>_<n>.json` suffix already present in `dir`, or 0 if
/// none exist (or the directory doesn't exist yet).
fn max_existing_index(dir: &Path, prefix: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let file_prefix = format!("{}_", prefix);
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let rest = name.strip_prefix(&file_prefix)?;
            let n = rest.strip_suffix(".json")?;
            n.parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0)
}

/// Response headers whose value belongs to *this one exchange*, not to
/// replays of the recorded mock: a `date` and `server` from the moment of
/// recording, re-served forever, would misdescribe every future replay as
/// having happened right then (shared with #106's `content-encoding` case
/// below).
const NON_REPLAYABLE_RESPONSE_HEADERS: &[&str] = &["date", "server"];

/// Response headers worth persisting into a recorded mock's
/// `response_headers`: not hop-by-hop (already stripped by [`forward`]), not
/// carrying credentials the upstream set for this specific exchange (e.g.
/// `Set-Cookie`), and not a per-exchange value a replay must not re-serve
/// (see [`NON_REPLAYABLE_RESPONSE_HEADERS`], and `content-encoding` —
/// stripped where the body is decoded, in [`record_exchange`]).
fn recordable_response_headers(headers: &HeaderMap) -> Option<HashMap<String, String>> {
    let map: HashMap<String, String> = headers
        .iter()
        .filter(|(name, _)| {
            !is_sensitive_header(name.as_str())
                && !NON_REPLAYABLE_RESPONSE_HEADERS.contains(&name.as_str())
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect();
    (!map.is_empty()).then_some(map)
}

fn build_query_matcher(query_params: &HashMap<String, String>) -> Option<QueryParamMatcher> {
    if query_params.is_empty() {
        return None;
    }
    Some(QueryParamMatcher {
        params: query_params
            .iter()
            .map(|(k, v)| (k.clone(), QueryParamValue::Exact(v.clone())))
            .collect(),
        strict: false,
    })
}

/// Environment variable naming extra request headers a recording should
/// require, comma-separated — additive to [`DEFAULT_RECORD_MATCH_HEADERS`],
/// for an API that genuinely varies its response by a header (e.g.
/// `MIMIC_RECORD_MATCH_HEADERS=x-tenant,accept`).
pub const RECORD_MATCH_HEADERS_ENV: &str = "MIMIC_RECORD_MATCH_HEADERS";

/// Header names captured into every recording's matcher by default.
///
/// `content-type` is the one header that plausibly *selects* which response
/// an endpoint should give; everything else a client sends — trace ids,
/// `origin`/`referer`, `accept-language`, `sec-fetch-*` — varies per request
/// for most real clients, and pinning it as `required` is what made a
/// recording stop matching the very next request (#105).
const DEFAULT_RECORD_MATCH_HEADERS: &[&str] = &["content-type"];

/// `MIMIC_RECORD_MATCH_HEADERS`, parsed once: lowercased header names to
/// capture into a recording's matcher, beyond [`DEFAULT_RECORD_MATCH_HEADERS`].
fn configured_record_match_headers() -> &'static HashSet<String> {
    static EXTRA: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    EXTRA.get_or_init(|| {
        std::env::var(RECORD_MATCH_HEADERS_ENV)
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

/// True when `name` (already lowercase) belongs in a recorded mock's header
/// matcher — [`DEFAULT_RECORD_MATCH_HEADERS`], or added via
/// [`RECORD_MATCH_HEADERS_ENV`].
fn is_recordable_header(name: &str) -> bool {
    DEFAULT_RECORD_MATCH_HEADERS.contains(&name) || configured_record_match_headers().contains(name)
}

/// Header matcher captured from the proxied request.
///
/// An *allowlist*, not a denylist: only [`is_recordable_header`] names become
/// `required` matchers. Sensitive headers (`Authorization`, `Cookie`, …) are
/// excluded outright so a recorded mock file is never an accidental secrets
/// store, regardless of what `MIMIC_RECORD_MATCH_HEADERS` names.
fn build_header_matcher(headers: &HashMap<String, String>) -> Option<HeaderMatcher> {
    let required: HashMap<String, HeaderValue> = headers
        .iter()
        .filter(|(k, _)| !is_sensitive_header(k) && is_recordable_header(k))
        .map(|(k, v)| (k.clone(), HeaderValue::Exact(v.clone())))
        .collect();
    if required.is_empty() {
        return None;
    }
    Some(HeaderMatcher {
        required,
        forbidden: Vec::new(),
        strict: false,
    })
}

fn build_body_matcher(body: Option<&Bytes>, content_type: Option<&str>) -> Option<BodyMatcher> {
    let body = body.filter(|b| !b.is_empty())?;
    match parse_body(body, content_type) {
        ParsedBody::Json(value) => Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: Some(value),
            partial: None,
            strict: false,
        })),
        ParsedBody::Text(text) => Some(BodyMatcher::Text(TextBodyMatcher {
            exact: Some(text),
            contains: None,
            regex: None,
        })),
        ParsedBody::Form(fields) => Some(BodyMatcher::Form(FormBodyMatcher {
            fields,
            strict: false,
        })),
        // No text representation to match on; still forwarded to the client,
        // just not turned into a matcher.
        ParsedBody::Binary(_) | ParsedBody::Empty => None,
    }
}

/// Response body as a `MockConfig.response` value: parsed JSON when the
/// content type says so, otherwise the raw text (matching how
/// `handler::build_response_parts` serves a non-JSON string response body).
fn response_value(body: &Bytes, content_type: Option<&str>) -> serde_json::Value {
    let is_json = content_type.is_some_and(|ct| ct.to_ascii_lowercase().contains("json"));
    let text = String::from_utf8_lossy(body).to_string();
    if is_json {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
    } else {
        serde_json::Value::String(text)
    }
}

fn build_recorded_mock(
    context: &RequestContext,
    status: u16,
    response_headers: &HeaderMap,
    response_body: &Bytes,
) -> MockConfig {
    let response_content_type = response_headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    MockConfig {
        method: context.method.clone(),
        path: context.path.clone(),
        status,
        response: response_value(response_body, response_content_type),
        response_file: None,
        template: None,
        response_bytes: None,
        consume_body: false,
        query_params: build_query_matcher(&context.query_params),
        headers: build_header_matcher(&context.headers),
        body: build_body_matcher(context.body.as_ref(), context.content_type.as_deref()),
        response_headers: recordable_response_headers(response_headers),
        delay_ms: None,
        sequence: None,
        tags: Vec::new(),
        source: None,
    }
}

/// Record one proxied exchange as a new mock file under `mocks_dir`, unless
/// it's a duplicate of one already recorded this run or the upstream
/// response isn't a content type worth recording (see
/// [`is_recordable_content_type`]).
///
/// Intended to run detached (`tokio::spawn`) from the request path: nothing
/// here blocks the client's response, including the directory scan
/// [`RecordState::reserve`] may need on a prefix's first use.
pub async fn record_exchange(
    record_state: std::sync::Arc<RecordState>,
    mocks_dir: PathBuf,
    context: RequestContext,
    status: u16,
    response_headers: HeaderMap,
    response_body: Bytes,
) {
    let response_content_type = response_headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if !is_recordable_content_type(response_content_type) {
        debug!(
            "Not recording {} {}: non-text upstream content-type {:?}",
            context.method, context.path, response_content_type
        );
        return;
    }

    // `forward` already strips `content-encoding` for anything it decoded
    // (see `DECODED_CONTENT_ENCODINGS`), so a value still here names an
    // encoding this build can't decode — recording it would mean writing the
    // still-compressed bytes as "text" (#106's mojibake) rather than a
    // silently corrupt mock.
    if let Some(encoding) = response_headers.get(CONTENT_ENCODING) {
        warn!(
            "Not recording {} {}: response content-encoding '{}' can't be decoded by this build, \
             so the body isn't text Mimic can record",
            context.method,
            context.path,
            encoding.to_str().unwrap_or("<non-utf8>")
        );
        return;
    }

    let signature = request_signature(&context);
    let sanitized_path = sanitize_path_for_filename(&context.path);
    let prefix = format!("{}_{}", context.method.to_lowercase(), sanitized_path);

    let Some(target_path) = record_state.reserve(&signature, &prefix, &mocks_dir).await else {
        debug!(
            "Not recording {} {}: already recorded this request shape",
            context.method, context.path
        );
        return;
    };

    let mock = build_recorded_mock(&context, status, &response_headers, &response_body);

    let write_target = target_path.clone();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(parent) = write_target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&mock)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&write_target, json)
    })
    .await;

    match result {
        Ok(Ok(())) => info!(
            "Recorded {} {} -> {}",
            context.method,
            context.path,
            target_path.display()
        ),
        Ok(Err(e)) => warn!(
            "Failed to write recorded mock {}: {}",
            target_path.display(),
            e
        ),
        Err(e) => warn!(
            "Recording task for {} panicked: {}",
            target_path.display(),
            e
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue as HttpHeaderValue;

    fn ctx(method: &str, path: &str) -> RequestContext {
        RequestContext {
            method: method.to_string(),
            path: path.to_string(),
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            query_params_all: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            content_type: None,
        }
    }

    // ---------------------------------------------------------------------
    // validate_upstream / configured_proxy_config
    // ---------------------------------------------------------------------

    #[test]
    fn validate_upstream_accepts_a_normal_url() {
        assert!(validate_upstream("https://api.stripe.com", 8080).is_ok());
    }

    #[test]
    fn validate_upstream_rejects_self_referential_loopback() {
        assert!(validate_upstream("http://localhost:8080", 8080).is_err());
        assert!(validate_upstream("http://127.0.0.1:8080", 8080).is_err());
    }

    #[test]
    fn validate_upstream_allows_loopback_on_a_different_port() {
        assert!(validate_upstream("http://localhost:9090", 8080).is_ok());
    }

    #[test]
    fn validate_upstream_rejects_non_http_schemes() {
        assert!(validate_upstream("ftp://example.com", 8080).is_err());
    }

    #[test]
    fn validate_upstream_rejects_garbage() {
        assert!(validate_upstream("not a url", 8080).is_err());
    }

    // All three of `configured_proxy_config`'s env vars are process-global, so
    // every scenario that touches them lives in this one test — split across
    // separate `#[test]` functions, Rust's parallel test runner would race
    // the same three env vars against each other (mirrors the handler's
    // MIMIC_ACTIVE_TAGS test, which notes the same constraint).
    #[test]
    fn configured_proxy_config_env_var_behavior() {
        let restore_upstream = std::env::var(PROXY_UPSTREAM_ENV).ok();
        let restore_record = std::env::var(RECORD_UPSTREAM_ENV).ok();
        let restore_timeout = std::env::var(PROXY_TIMEOUT_MS_ENV).ok();

        std::env::remove_var(PROXY_UPSTREAM_ENV);
        std::env::remove_var(RECORD_UPSTREAM_ENV);
        std::env::remove_var(PROXY_TIMEOUT_MS_ENV);
        assert!(
            configured_proxy_config(8080).is_none(),
            "unset MIMIC_PROXY_UPSTREAM must mean proxying is off"
        );

        std::env::set_var(PROXY_UPSTREAM_ENV, "https://api.example.com/");
        std::env::set_var(RECORD_UPSTREAM_ENV, "true");
        std::env::set_var(PROXY_TIMEOUT_MS_ENV, "1234");
        let cfg = configured_proxy_config(8080).expect("proxy should be configured");
        assert_eq!(cfg.upstream, "https://api.example.com");
        assert!(cfg.record);
        assert_eq!(cfg.timeout_ms, 1234);

        std::env::set_var(PROXY_UPSTREAM_ENV, "http://localhost:8080");
        assert!(
            configured_proxy_config(8080).is_none(),
            "an upstream that loops back to Mimic's own port must disable proxying"
        );

        match restore_upstream {
            Some(v) => std::env::set_var(PROXY_UPSTREAM_ENV, v),
            None => std::env::remove_var(PROXY_UPSTREAM_ENV),
        }
        match restore_record {
            Some(v) => std::env::set_var(RECORD_UPSTREAM_ENV, v),
            None => std::env::remove_var(RECORD_UPSTREAM_ENV),
        }
        match restore_timeout {
            Some(v) => std::env::set_var(PROXY_TIMEOUT_MS_ENV, v),
            None => std::env::remove_var(PROXY_TIMEOUT_MS_ENV),
        }
    }

    // ---------------------------------------------------------------------
    // sanitize_path_for_filename
    // ---------------------------------------------------------------------

    #[test]
    fn sanitize_path_strips_slashes_and_collapses_separators() {
        assert_eq!(sanitize_path_for_filename("/users/42"), "users_42");
        assert_eq!(sanitize_path_for_filename("/v1/charges"), "v1_charges");
        assert_eq!(sanitize_path_for_filename("/"), "root");
        assert_eq!(sanitize_path_for_filename(""), "root");
    }

    #[test]
    fn sanitize_path_replaces_special_characters() {
        assert_eq!(sanitize_path_for_filename("/search?weird"), "search_weird");
        assert_eq!(sanitize_path_for_filename("/a..b"), "a_b");
    }

    // ---------------------------------------------------------------------
    // request_signature
    // ---------------------------------------------------------------------

    #[test]
    fn request_signature_is_stable_for_identical_requests() {
        let a = ctx("GET", "/users/1");
        let b = ctx("GET", "/users/1");
        assert_eq!(request_signature(&a), request_signature(&b));
    }

    #[test]
    fn request_signature_differs_on_method_or_path() {
        let get = ctx("GET", "/users/1");
        let post = ctx("POST", "/users/1");
        let other_path = ctx("GET", "/users/2");
        assert_ne!(request_signature(&get), request_signature(&post));
        assert_ne!(request_signature(&get), request_signature(&other_path));
    }

    #[test]
    fn request_signature_ignores_sensitive_and_noise_headers() {
        let mut a = ctx("GET", "/users/1");
        a.headers
            .insert("authorization".to_string(), "Bearer abc".to_string());
        a.headers
            .insert("user-agent".to_string(), "curl/8.0".to_string());

        let mut b = ctx("GET", "/users/1");
        b.headers
            .insert("authorization".to_string(), "Bearer xyz".to_string());
        b.headers
            .insert("user-agent".to_string(), "curl/9.0".to_string());

        assert_eq!(request_signature(&a), request_signature(&b));
    }

    #[test]
    fn request_signature_differs_on_content_type() {
        // content-type is the one header captured by default (#105) — it
        // plausibly selects which response an endpoint gives.
        let mut a = ctx("GET", "/users/1");
        a.headers
            .insert("content-type".to_string(), "application/json".to_string());

        let mut b = ctx("GET", "/users/1");
        b.headers
            .insert("content-type".to_string(), "text/plain".to_string());

        assert_ne!(request_signature(&a), request_signature(&b));
    }

    /// The #105 repro: a recording made against a request carrying a trace id
    /// must still dedupe with — and its matcher must still accept — the next
    /// request that differs only in that header.
    #[test]
    fn request_signature_ignores_trace_and_browser_headers_by_default() {
        let mut a = ctx("GET", "/users/1");
        a.headers
            .insert("x-request-id".to_string(), "abc-111".to_string());
        a.headers
            .insert("traceparent".to_string(), "00-aaa-bbb-01".to_string());
        a.headers
            .insert("referer".to_string(), "http://localhost:3000/".to_string());
        a.headers
            .insert("origin".to_string(), "http://localhost:3000".to_string());
        a.headers
            .insert("accept-language".to_string(), "en-US".to_string());

        let mut b = ctx("GET", "/users/1");
        b.headers
            .insert("x-request-id".to_string(), "def-222".to_string());
        b.headers
            .insert("traceparent".to_string(), "00-ccc-ddd-01".to_string());
        b.headers.insert(
            "referer".to_string(),
            "http://localhost:3000/page".to_string(),
        );
        b.headers
            .insert("origin".to_string(), "http://localhost:3000".to_string());
        b.headers
            .insert("accept-language".to_string(), "fr-FR".to_string());

        assert_eq!(
            request_signature(&a),
            request_signature(&b),
            "none of these should make two otherwise-identical requests dedupe separately"
        );
    }

    // ---------------------------------------------------------------------
    // is_recordable_content_type
    // ---------------------------------------------------------------------

    #[test]
    fn recordable_content_types() {
        assert!(is_recordable_content_type(Some("application/json")));
        assert!(is_recordable_content_type(Some(
            "application/json; charset=utf-8"
        )));
        assert!(is_recordable_content_type(Some("text/plain")));
        assert!(is_recordable_content_type(Some("application/xml")));
        assert!(is_recordable_content_type(None));
    }

    #[test]
    fn non_recordable_content_types() {
        assert!(!is_recordable_content_type(Some("image/png")));
        assert!(!is_recordable_content_type(Some("application/pdf")));
        assert!(!is_recordable_content_type(Some(
            "application/octet-stream"
        )));
    }

    // ---------------------------------------------------------------------
    // build_recorded_mock: the sensitive-header redaction acceptance
    // criterion
    // ---------------------------------------------------------------------

    #[test]
    fn recorded_mock_never_captures_sensitive_request_headers() {
        let mut context = ctx("GET", "/secrets");
        context.headers.insert(
            "authorization".to_string(),
            "Bearer super-secret".to_string(),
        );
        context
            .headers
            .insert("cookie".to_string(), "session=abc123".to_string());
        context
            .headers
            .insert("content-type".to_string(), "application/json".to_string());

        let mock = build_recorded_mock(&context, 200, &HeaderMap::new(), &Bytes::new());

        let headers = mock.headers.expect("content-type should still be captured");
        assert!(!headers.required.contains_key("authorization"));
        assert!(!headers.required.contains_key("cookie"));
        assert!(headers.required.contains_key("content-type"));
    }

    // ---------------------------------------------------------------------
    // build_header_matcher: the allowlist (#105)
    // ---------------------------------------------------------------------

    /// The other half of the #105 repro: everything a recording used to pin
    /// by default is exactly what stopped it from ever replaying.
    #[test]
    fn recorded_mock_does_not_capture_trace_ids_or_browser_headers_by_default() {
        let mut context = ctx("GET", "/api/items");
        context
            .headers
            .insert("x-request-id".to_string(), "abc-111".to_string());
        context
            .headers
            .insert("traceparent".to_string(), "00-aaa-bbb-01".to_string());
        context
            .headers
            .insert("origin".to_string(), "http://localhost:3000".to_string());
        context
            .headers
            .insert("referer".to_string(), "http://localhost:3000/".to_string());
        context
            .headers
            .insert("accept-language".to_string(), "en-US".to_string());
        context
            .headers
            .insert("sec-fetch-mode".to_string(), "cors".to_string());

        let mock = build_recorded_mock(&context, 200, &HeaderMap::new(), &Bytes::new());

        assert!(
            mock.headers.is_none(),
            "none of these headers plausibly select a response, so the recording \
             must carry no header matcher at all"
        );
    }

    #[test]
    fn recorded_mock_captures_content_type_by_default() {
        let mut context = ctx("POST", "/api/items");
        context
            .headers
            .insert("content-type".to_string(), "application/json".to_string());

        let mock = build_recorded_mock(&context, 200, &HeaderMap::new(), &Bytes::new());

        let headers = mock.headers.expect("content-type should be captured");
        match headers.required.get("content-type") {
            Some(HeaderValue::Exact(value)) => assert_eq!(value, "application/json"),
            other => panic!("expected an exact content-type matcher, got {:?}", other),
        }
    }

    #[test]
    fn recorded_mock_never_captures_sensitive_response_headers() {
        let context = ctx("GET", "/login");
        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            "set-cookie",
            HttpHeaderValue::from_static("session=xyz; HttpOnly"),
        );
        response_headers.insert(
            CONTENT_TYPE,
            HttpHeaderValue::from_static("application/json"),
        );

        let mock = build_recorded_mock(
            &context,
            200,
            &response_headers,
            &Bytes::from_static(b"{\"ok\":true}"),
        );

        let headers = mock
            .response_headers
            .expect("content-type should be captured");
        assert!(!headers.contains_key("set-cookie"));
        assert!(headers.contains_key("content-type"));
    }

    #[test]
    fn recorded_mock_parses_json_response_body() {
        let context = ctx("GET", "/users/1");
        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            CONTENT_TYPE,
            HttpHeaderValue::from_static("application/json"),
        );

        let mock = build_recorded_mock(
            &context,
            200,
            &response_headers,
            &Bytes::from_static(b"{\"id\":1,\"name\":\"Alice\"}"),
        );

        assert_eq!(mock.response["id"], 1);
        assert_eq!(mock.response["name"], "Alice");
    }

    #[test]
    fn recorded_mock_keeps_non_json_response_as_a_string() {
        let context = ctx("GET", "/data.xml");
        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            CONTENT_TYPE,
            HttpHeaderValue::from_static("application/xml"),
        );

        let mock = build_recorded_mock(
            &context,
            200,
            &response_headers,
            &Bytes::from_static(b"<ok/>"),
        );

        assert_eq!(mock.response, serde_json::json!("<ok/>"));
    }

    #[test]
    fn recorded_mock_captures_query_params_as_exact_matchers() {
        let mut context = ctx("GET", "/search");
        context
            .query_params
            .insert("q".to_string(), "rust".to_string());

        let mock = build_recorded_mock(&context, 200, &HeaderMap::new(), &Bytes::new());

        let qp = mock.query_params.expect("query params should be captured");
        assert!(matches!(
            qp.params.get("q"),
            Some(QueryParamValue::Exact(v)) if v == "rust"
        ));
    }

    #[test]
    fn recorded_mock_has_no_tags_so_it_is_immediately_matchable() {
        let context = ctx("GET", "/x");
        let mock = build_recorded_mock(&context, 200, &HeaderMap::new(), &Bytes::new());
        assert!(mock.tags.is_empty());
    }

    // ---------------------------------------------------------------------
    // RecordState: dedupe by signature, incrementing filenames otherwise
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn record_state_reserves_incrementing_filenames_for_distinct_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let state = RecordState::new();

        let first = state
            .reserve("sig-a", "get_users", dir.path())
            .await
            .unwrap();
        let second = state
            .reserve("sig-b", "get_users", dir.path())
            .await
            .unwrap();

        assert_eq!(first, dir.path().join("get_users_1.json"));
        assert_eq!(second, dir.path().join("get_users_2.json"));
    }

    #[tokio::test]
    async fn record_state_dedupes_identical_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let state = RecordState::new();

        let first = state.reserve("same-sig", "get_users", dir.path()).await;
        let second = state.reserve("same-sig", "get_users", dir.path()).await;

        assert!(first.is_some());
        assert!(
            second.is_none(),
            "a repeated signature must not reserve a second file"
        );
    }

    #[tokio::test]
    async fn record_state_continues_numbering_from_files_already_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("get_users_1.json"), "{}").unwrap();
        std::fs::write(dir.path().join("get_users_3.json"), "{}").unwrap();

        let state = RecordState::new();
        let next = state
            .reserve("new-sig", "get_users", dir.path())
            .await
            .unwrap();

        assert_eq!(next, dir.path().join("get_users_4.json"));
    }

    #[tokio::test]
    async fn record_state_concurrent_identical_requests_reserve_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(RecordState::new());

        let mut handles = Vec::new();
        for _ in 0..20 {
            let state = state.clone();
            let dir_path = dir.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                state.reserve("race-sig", "post_login", &dir_path).await
            }));
        }

        let mut reserved = 0;
        for handle in handles {
            if handle.await.unwrap().is_some() {
                reserved += 1;
            }
        }

        assert_eq!(
            reserved, 1,
            "only one of the concurrent identical requests should win the reservation"
        );
    }
}
