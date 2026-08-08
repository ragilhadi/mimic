use crate::matcher::{
    explain_match, explain_no_match, find_matching_mock, is_pattern_path, parse_body,
    parse_headers, parse_query_string, requires_body, MatchResult, RequestContext,
};
use crate::template::{render_response, TemplateContext};
use crate::types::{
    is_sensitive_header, parse_active_tags, ActiveTags, MockConfig, MockIdentity, MockStore,
    RequestLog, RequestRecord, SequenceCounters, SequenceStep,
};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, Method, StatusCode, Uri},
    response::{Html, IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Per-mock hit counts, keyed like the sequence counters — by the mock's
/// stable [`MockIdentity`] — so one mock among several registered for a path
/// can be told from the rest, and keeps its count across a hot reload.
pub type MockHits = Arc<tokio::sync::RwLock<HashMap<MockIdentity, u64>>>;

#[derive(Clone)]
pub struct AppState {
    pub mocks: MockStore,
    pub request_log: RequestLog,
    pub request_counter: Arc<AtomicU64>,
    pub sequence_counters: SequenceCounters,
    /// How many requests each mock has served, surfaced by `/admin/mocks`
    pub mock_hits: MockHits,
    /// When the process started serving, surfaced as uptime by `/health`
    pub started_at: std::time::Instant,
    /// Which scenario tags are active, or `None` for "no filter" — the
    /// default. Read on every request, rewritten by `POST /admin/scenario`.
    pub active_tags: ActiveTags,
}

impl AppState {
    /// State with no scenario filter: every loaded mock is matchable.
    // The server itself goes through `with_active_tags` so `MIMIC_ACTIVE_TAGS`
    // is honored; this stays as the no-scenario constructor tests build on.
    #[allow(dead_code)]
    pub fn new(mocks: MockStore) -> Self {
        Self::with_active_tags(mocks, None)
    }

    /// [`AppState::new`] with a starting scenario selection, as
    /// `MIMIC_ACTIVE_TAGS` provides at startup.
    pub fn with_active_tags(mocks: MockStore, active_tags: Option<HashSet<String>>) -> Self {
        Self {
            mocks,
            request_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            request_counter: Arc::new(AtomicU64::new(0)),
            sequence_counters: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            mock_hits: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            started_at: std::time::Instant::now(),
            active_tags: Arc::new(tokio::sync::RwLock::new(active_tags)),
        }
    }
}

/// Default cap on the request body Mimic will buffer (10 MB).
const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Environment variable overriding [`DEFAULT_MAX_BODY_SIZE`], in bytes.
const MAX_BODY_SIZE_ENV: &str = "MIMIC_MAX_BODY_SIZE";

/// The configured maximum request body size, in bytes.
///
/// Read once from `MIMIC_MAX_BODY_SIZE` on first use; an unset, unparsable,
/// or zero value falls back to [`DEFAULT_MAX_BODY_SIZE`]. The limit is
/// enforced *while* the body streams in (see [`handle_request`]), so a
/// request larger than this never gets fully buffered.
pub fn max_body_size() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| match std::env::var(MAX_BODY_SIZE_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                warn!(
                    "Invalid {}='{}', falling back to {} bytes",
                    MAX_BODY_SIZE_ENV, raw, DEFAULT_MAX_BODY_SIZE
                );
                DEFAULT_MAX_BODY_SIZE
            }
        },
        Err(_) => DEFAULT_MAX_BODY_SIZE,
    })
}

/// Default cap on how many requests the in-memory log keeps (see
/// [`max_log_entries`]).
const DEFAULT_MAX_LOG_ENTRIES: usize = 1000;

/// Environment variable overriding [`DEFAULT_MAX_LOG_ENTRIES`].
const MAX_LOG_ENTRIES_ENV: &str = "MIMIC_MAX_LOG_ENTRIES";

/// How many requests the log retains before the oldest are dropped.
///
/// The log lives in memory and the dashboard re-reads it every few seconds, so
/// an unbounded log is a slow leak that degrades the very UI meant to observe
/// it. Read once from `MIMIC_MAX_LOG_ENTRIES`; `0` disables the bound for the
/// rare run that genuinely wants every request kept.
pub fn max_log_entries() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| match std::env::var(MAX_LOG_ENTRIES_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                warn!(
                    "Invalid {}='{}', falling back to {} entries",
                    MAX_LOG_ENTRIES_ENV, raw, DEFAULT_MAX_LOG_ENTRIES
                );
                DEFAULT_MAX_LOG_ENTRIES
            }
        },
        Err(_) => DEFAULT_MAX_LOG_ENTRIES,
    })
}

/// Default cap on the request/response bodies stored per log entry (64 KB).
const DEFAULT_MAX_RECORDED_BODY: usize = 64 * 1024;

/// Environment variable overriding [`DEFAULT_MAX_RECORDED_BODY`], in bytes.
const MAX_RECORDED_BODY_ENV: &str = "MIMIC_MAX_RECORDED_BODY";

/// How much of a body is kept on a log entry, in bytes.
///
/// `MIMIC_MAX_BODY_SIZE` caps what Mimic will *read*; it says nothing about
/// what it keeps. Without a second cap a single 10 MB mock response would be
/// copied into every log entry that hit it. `0` disables truncation.
pub fn max_recorded_body_size() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| match std::env::var(MAX_RECORDED_BODY_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                warn!(
                    "Invalid {}='{}', falling back to {} bytes",
                    MAX_RECORDED_BODY_ENV, raw, DEFAULT_MAX_RECORDED_BODY
                );
                DEFAULT_MAX_RECORDED_BODY
            }
        },
        Err(_) => DEFAULT_MAX_RECORDED_BODY,
    })
}

/// Marker appended to a body the log had to cut short.
pub const TRUNCATION_MARKER: &str = "…[truncated]";

/// Cut `body` down to `limit` bytes for storage, marking it when anything was
/// dropped. `limit` of 0 keeps the body whole.
///
/// The cut lands on a character boundary, so a body ending mid-multi-byte
/// character still round-trips as valid UTF-8 through the admin API.
pub fn truncate_body(body: String, limit: usize) -> String {
    if limit == 0 || body.len() <= limit {
        return body;
    }
    let mut end = limit;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = body[..end].to_string();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

/// Append `record` to `log`, dropping the oldest entries once `cap` is
/// exceeded. A `cap` of 0 leaves the log unbounded.
fn push_bounded(log: &mut Vec<RequestRecord>, record: RequestRecord, cap: usize) {
    log.push(record);
    if cap > 0 && log.len() > cap {
        log.drain(..log.len() - cap);
    }
}

/// The port the server is configured to listen on.
///
/// Lives here so `/health` reports the same number `main` binds to rather than
/// re-deriving it and risking disagreement.
pub fn configured_port() -> u16 {
    std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .unwrap_or(8080)
}

/// Environment variable selecting the scenario active at startup.
pub const ACTIVE_TAGS_ENV: &str = "MIMIC_ACTIVE_TAGS";

/// The scenario the server starts in, read from `MIMIC_ACTIVE_TAGS`
/// (comma-separated). Unset, empty, or all-whitespace means no filter, i.e.
/// every mock is matchable — the behavior of every Mimic before scenarios
/// existed.
pub fn configured_active_tags() -> Option<HashSet<String>> {
    parse_active_tags(std::env::var(ACTIVE_TAGS_ENV).ok())
}

pub async fn handle_request(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    State(state): State<AppState>,
    body: Body,
) -> Response {
    let path = uri.path().to_string();
    let method_str = method.as_str().to_string();

    debug!("Incoming request: {} {}", method_str, uri);

    // Parse query parameters from URI
    let query_params = parse_query_string(uri.query());

    // Parse headers into HashMap (normalized to lowercase)
    let parsed_headers = parse_headers(&headers);

    // Get content type for body parsing
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // The scenario selection is snapshotted once, up front, so every decision
    // this request makes — whether to read the body, which mock wins, why
    // nothing did — is taken against the same set of active tags even if
    // `POST /admin/scenario` lands halfway through.
    let active_tags = state.active_tags.read().await.clone();

    // Decide whether to read the body, scoped to the mocks that could actually
    // serve this method+path — a `body` matcher or `consume_body: true` on some
    // *other* endpoint is none of this request's business (acquire read lock)
    let needs_body = {
        let mocks = state.mocks.read().await;
        requires_body(&method_str, &path, &mocks, active_tags.as_ref())
    };

    // The body is wrapped in `Limited` so the stream is cut off — and the
    // request rejected with 413 — as soon as it exceeds the cap, rather than
    // being buffered in full and only then measured.
    let max_body = max_body_size();
    let body_bytes: Option<Bytes> = if needs_body {
        match Limited::new(body, max_body).collect().await {
            Ok(collected) => {
                let bytes = collected.to_bytes();
                debug!("Consumed {} bytes from request body", bytes.len());
                Some(bytes)
            }
            Err(e) if is_length_limit(&*e) => {
                warn!(
                    "Rejecting {} {}: request body exceeds the {}-byte limit",
                    method_str, path, max_body
                );
                return payload_too_large(&method_str, &path, max_body);
            }
            Err(e) => {
                debug!("Failed to read body: {}", e);
                None
            }
        }
    } else {
        debug!(
            "Skipping request body: no mock for {} {} needs it",
            method_str, path
        );
        None
    };

    // Build request context for matching
    let mut context = RequestContext {
        method: method_str.clone(),
        path: path.clone(),
        path_params: HashMap::new(),
        query_params,
        headers: parsed_headers.clone(),
        body: body_bytes,
        content_type,
    };

    // Find matching mock using the matcher (acquire read lock)
    let mocks = state.mocks.read().await;
    let matched = find_matching_mock(&context, &mocks, active_tags.as_ref());
    // Release read lock before any await (counter lock, recording, delay)
    drop(mocks);

    match matched {
        Some(result) => {
            // Built before the result is taken apart: it reads the breakdown
            // recorded when the score was computed, so the arithmetic shown on
            // the dashboard is the arithmetic that actually ran.
            let match_explanation = explain_match(&result);
            let match_score = result.score;
            let MatchResult {
                mock,
                index,
                path_params,
                matched_key: mock_key,
                ..
            } = result;

            // Named path parameters captured from the mock's pattern (e.g.
            // `/users/:id`), if any, become available to templating below.
            context.path_params = path_params;

            // The identity this mock's cross-request state hangs off: its
            // declared key plus the file it came from, not its position in a
            // vector that hot reload rebuilds every two seconds. Computed once
            // and used for both the sequence counter and the hit count, which
            // must agree on which mock they're talking about.
            //
            // Keying by the *declared* path (`mock_key`) rather than the
            // concrete request path is what makes a pattern mock like
            // `/users/:id` advance a single shared sequence regardless of which
            // id was requested.
            let identity = MockIdentity::of(&mock, &mock_key, index);

            // Resolve the response: sequence step if configured, top-level otherwise.
            // An empty sequence array falls back to the top-level status/response.
            let (status_u16, response, delay_ms) = match mock.sequence.as_deref() {
                Some(steps) if !steps.is_empty() => {
                    advance_sequence(&state.sequence_counters, &identity, steps).await
                }
                _ => (mock.status, mock.response.clone(), None),
            };

            info!("Mock matched: {} {} -> {}", method_str, path, status_u16);

            // Interpolate {{path.X}}, {{query.X}}, {{header.X}}, {{body.X}} in the
            // response using data from the matched request, before it's consumed
            // by recording below.
            let parsed_body = context
                .body
                .as_ref()
                .map(|b| parse_body(b, context.content_type.as_deref()));
            let template_ctx = TemplateContext {
                path_params: &context.path_params,
                query_params: &context.query_params,
                headers: &context.headers,
                body: parsed_body.as_ref(),
            };
            let response = render_response(&response, &template_ctx);

            let status = StatusCode::from_u16(status_u16).unwrap_or(StatusCode::OK);
            let matched_key = format!("{}:{}", method_str, path);

            // Count the hit against the specific mock that served it, keyed the
            // same way as its sequence counter so several mocks sharing a path
            // stay distinguishable.
            {
                let mut hits = state.mock_hits.write().await;
                *hits.entry(identity).or_insert(0) += 1;
            }

            // Serialize the response once: the same bytes are both sent to the
            // client and recorded, so the drawer can't show something the
            // client didn't get.
            let (header_map, body) =
                build_response_parts(&response, mock.response_headers.as_ref());

            // Record the request with the status and response actually served
            let path_params = context.path_params.clone();
            record_request(
                &state,
                context,
                RequestOutcome {
                    matched_mock: Some(matched_key),
                    response_status: status_u16,
                    response_body: Some(body.clone()),
                    response_headers: recorded_response_headers(&header_map),
                    match_score: Some(match_score),
                    path_params,
                    match_explanation: Some(match_explanation),
                },
            )
            .await;

            // Resolve the delay: a sequence step's own delay wins, otherwise the
            // mock-level delay_ms (fixed or sampled from a range) applies
            let effective_delay = delay_ms.or_else(|| mock.delay_ms.as_ref().map(|d| d.resolve()));

            // Apply the delay last, with no locks held
            if let Some(ms) = effective_delay {
                if ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
            }

            // Return configured response with any custom headers
            assemble_response(status, header_map, body)
        }
        None => {
            info!("No mock found for: {} {}", method_str, path);

            // Diagnose the miss. Only reached on the 404 path — it walks the
            // mock map and formats strings, which the matched path must not pay
            // for.
            let explanation = {
                let mocks = state.mocks.read().await;
                explain_no_match(&context, &mocks, active_tags.as_ref())
            };
            if let Some(ref why) = explanation {
                debug!("No match for {} {}: {}", method_str, path, why);
            }

            // Record the request (clone query_params for use in error response)
            let query_params_clone = context.query_params.clone();
            let error_body = json!({
                "error": "mock not found",
                "method": method_str,
                "path": path,
                "query_params": query_params_clone,
                "headers_received": parsed_headers.keys().collect::<Vec<_>>()
            });

            record_request(
                &state,
                context,
                RequestOutcome {
                    matched_mock: None,
                    response_status: 404,
                    response_body: Some(error_body.to_string()),
                    response_headers: HashMap::new(),
                    match_score: None,
                    path_params: HashMap::new(),
                    match_explanation: explanation,
                },
            )
            .await;

            // Return 404 with detailed error message
            (StatusCode::NOT_FOUND, Json(error_body)).into_response()
        }
    }
}

/// The response headers to store on a log entry, with sensitive values
/// replaced.
///
/// Response headers are a new output surface. Request headers have always been
/// redacted on the way into the log; a mock is perfectly capable of returning
/// `Set-Cookie`, and without this it would walk straight into the dashboard.
fn recorded_response_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_str().to_string();
            let value = if is_sensitive_header(&name) {
                "[REDACTED]".to_string()
            } else {
                value.to_str().unwrap_or("[non-UTF-8]").to_string()
            };
            (name, value)
        })
        .collect()
}

/// True if `err`, or anything it wraps, is a body-length-limit error.
///
/// The source chain has to be walked rather than downcasting the outermost
/// error: the limit trips deep inside the body stream and surfaces re-boxed,
/// so a plain downcast only sees the outer layer — which is how an oversized
/// chunked body could be mistaken for "no body" and answered 200.
fn is_length_limit(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(e) = current {
        if e.is::<LengthLimitError>() {
            return true;
        }
        current = e.source();
    }
    false
}

/// 413 returned when a request body exceeds [`max_body_size`]. Mirrors the
/// shape of the 404 "mock not found" body so clients can parse either the
/// same way.
fn payload_too_large(method: &str, path: &str, max_body: usize) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({
            "error": "payload too large",
            "method": method,
            "path": path,
            "max_body_size": max_body
        })),
    )
        .into_response()
}

pub async fn health_check(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mocks = state.mocks.read().await;
    // `mocks_loaded` counts registered routes, which is what it has always
    // meant; `mock_count` is the number of mock definitions behind them, since
    // several can share one METHOD:path.
    let mock_count: usize = mocks.values().map(|list| list.len()).sum();
    Json(json!({
        "status": "healthy",
        "mocks_loaded": mocks.len(),
        "mock_count": mock_count,
        "service": "mimic",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "port": configured_port(),
        "max_body_size": max_body_size(),
        "max_log_entries": max_log_entries(),
        "max_recorded_body": max_recorded_body_size(),
        "requests_recorded": state.request_counter.load(Ordering::Relaxed),
    }))
}

/// Everything known about how a request was answered, handed to
/// [`record_request`] as one value so the recorder's signature doesn't grow a
/// row of same-typed positional arguments.
#[derive(Default)]
struct RequestOutcome {
    matched_mock: Option<String>,
    response_status: u16,
    response_body: Option<String>,
    response_headers: HashMap<String, String>,
    match_score: Option<u32>,
    path_params: HashMap<String, String>,
    match_explanation: Option<String>,
}

/// Record a request into the request log, redacting sensitive headers
async fn record_request(state: &AppState, context: RequestContext, outcome: RequestOutcome) {
    let redacted_headers = context
        .headers
        .into_iter()
        .map(|(k, v)| {
            if is_sensitive_header(&k) {
                (k, "[REDACTED]".to_string())
            } else {
                (k, v)
            }
        })
        .collect();

    let body_limit = max_recorded_body_size();
    let mut record = RequestRecord {
        id: 0,
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        method: context.method,
        path: context.path,
        query_params: context.query_params,
        headers: redacted_headers,
        body: context
            .body
            .and_then(|b| String::from_utf8(b.to_vec()).ok())
            .map(|b| truncate_body(b, body_limit)),
        matched_mock: outcome.matched_mock,
        response_status: outcome.response_status,
        response_body: outcome.response_body.map(|b| truncate_body(b, body_limit)),
        response_headers: outcome.response_headers,
        match_score: outcome.match_score,
        path_params: outcome.path_params,
        match_explanation: outcome.match_explanation,
    };
    // IDs are unique but may not be strictly sequential under concurrent load
    let mut log = state.request_log.write().await;
    record.id = state.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
    push_bounded(&mut log, record, max_log_entries());
}

#[derive(Deserialize, Default)]
pub struct RequestFilter {
    /// Substring of the request path, not an exact match — the dashboard's
    /// "Filter by path…" box has always read like a search, and now is one.
    pub path: Option<String>,
    pub method: Option<String>,
    /// An exact code (`404`) or a status class (`4xx`, `5xx`).
    pub status: Option<String>,
    /// Keep only requests that matched no mock. Accepts `true`/`1`/`yes`.
    pub unmatched_only: Option<String>,
    /// Case-insensitive free-text search over the body, headers, and query
    /// parameters of each recorded request.
    pub search: Option<String>,
}

/// True if `value` reads as an affirmative in a query string. An absent or
/// empty value is false, so `?unmatched_only=` behaves like "off" rather than
/// failing the whole request.
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// True if `status` satisfies a `status` filter of either an exact code
/// (`"404"`) or a class (`"4xx"`).
///
/// An unparsable filter matches nothing: silently ignoring it would show a
/// full log and read as "no filtering happened", which is exactly the
/// confusion this endpoint is meant to end.
fn status_matches(filter: &str, status: u16) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    let lowered = filter.to_ascii_lowercase();
    if let Some(class) = lowered.strip_suffix("xx") {
        return match class.parse::<u16>() {
            Ok(class) => status / 100 == class,
            Err(_) => false,
        };
    }
    match filter.parse::<u16>() {
        Ok(code) => status == code,
        Err(_) => false,
    }
}

/// True if `needle` (already lowercased) appears anywhere in the request's
/// body, headers, or query parameters.
///
/// Header *values* are searched as stored — which means a redacted credential
/// is `[REDACTED]` here too, and can't be recovered by guessing at it.
fn record_matches_search(record: &RequestRecord, needle: &str) -> bool {
    if let Some(ref body) = record.body {
        if body.to_lowercase().contains(needle) {
            return true;
        }
    }
    record
        .headers
        .iter()
        .any(|(k, v)| k.to_lowercase().contains(needle) || v.to_lowercase().contains(needle))
        || record
            .query_params
            .iter()
            .any(|(k, v)| k.to_lowercase().contains(needle) || v.to_lowercase().contains(needle))
}

pub async fn list_requests(
    State(state): State<AppState>,
    Query(filter): Query<RequestFilter>,
) -> Json<serde_json::Value> {
    let unmatched_only = filter.unmatched_only.as_deref().is_some_and(is_truthy);
    let search = filter
        .search
        .as_ref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    let log = state.request_log.read().await;
    let filtered: Vec<&RequestRecord> = log
        .iter()
        .filter(|r| {
            if let Some(ref p) = filter.path {
                if !p.is_empty() && !r.path.contains(p.as_str()) {
                    return false;
                }
            }
            if let Some(ref m) = filter.method {
                if !m.is_empty() && !r.method.eq_ignore_ascii_case(m) {
                    return false;
                }
            }
            if let Some(ref s) = filter.status {
                if !status_matches(s, r.response_status) {
                    return false;
                }
            }
            if unmatched_only && r.matched_mock.is_some() {
                return false;
            }
            if let Some(ref needle) = search {
                if !record_matches_search(r, needle) {
                    return false;
                }
            }
            true
        })
        .collect();

    Json(json!({
        "count": filtered.len(),
        "requests": filtered
    }))
}

/// One loaded mock as `/admin/mocks` reports it.
fn describe_mock_entry(
    key: &str,
    index: usize,
    mock: &MockConfig,
    hits: u64,
    active_tags: Option<&HashSet<String>>,
) -> serde_json::Value {
    json!({
        "key": key,
        "index": index,
        "method": mock.method,
        "path": mock.path,
        "status": mock.status,
        "source": mock.source,
        "has_path_params": is_pattern_path(&mock.path),
        "matchers": {
            "query_params": mock.query_params.is_some(),
            "headers": mock.headers.is_some(),
            "body": mock.body.is_some(),
        },
        "delay_ms": mock.delay_ms,
        "sequence_steps": mock.sequence.as_ref().map(|s| s.len()),
        "response_headers": mock.response_headers.as_ref().map_or(0, |h| h.len()),
        "consume_body": mock.consume_body,
        "hits": hits,
        // Scenario tags, plus whether the current scenario lets this mock be
        // matched at all — an inactive mock is loaded but answers nothing,
        // which is otherwise indistinguishable from a broken matcher.
        "tags": mock.tags,
        "active": mock.is_active(active_tags),
        // The whole config, so the dashboard's expand-a-row can show what the
        // file says without a second round trip
        "config": mock,
    })
}

/// `GET /admin/mocks` — every mock currently loaded, with its matchers, source
/// file, and hit count.
///
/// Reads through the same lock hot reload writes to, so what it reports is the
/// mock set serving requests right now, not a snapshot taken at startup.
pub async fn list_mocks(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mocks = state.mocks.read().await;
    let hits = state.mock_hits.read().await;
    let active_tags = state.active_tags.read().await;

    let mut keys: Vec<&String> = mocks.keys().collect();
    keys.sort();

    let mut entries: Vec<serde_json::Value> = Vec::new();
    for key in keys {
        for (index, mock) in mocks[key].iter().enumerate() {
            let hit_count = hits
                .get(&MockIdentity::of(mock, key, index))
                .copied()
                .unwrap_or(0);
            entries.push(describe_mock_entry(
                key,
                index,
                mock,
                hit_count,
                active_tags.as_ref(),
            ));
        }
    }

    Json(json!({
        "count": entries.len(),
        "mocks": entries,
    }))
}

/// `GET /admin/sequences` — where each stateful sequence currently stands.
///
/// `step` is the number of calls served so far, i.e. the index of the step the
/// *next* request will get; `total` is the sequence's length, read back from
/// the mock the counter belongs to.
pub async fn list_sequences(State(state): State<AppState>) -> Json<serde_json::Value> {
    let counters = state.sequence_counters.read().await;
    let mocks = state.mocks.read().await;

    let mut identities: Vec<&MockIdentity> = counters.keys().collect();
    identities.sort();

    let sequences: Vec<serde_json::Value> = identities
        .into_iter()
        .map(|identity| {
            // The mock the counter belongs to, found by identity rather than
            // by position — a counter outlives a reload, a position doesn't.
            // A counter whose mock is gone still renders, with a null `total`.
            let mock = mocks
                .get(&identity.key)
                .and_then(|list| {
                    list.iter().enumerate().find(|(index, mock)| {
                        MockIdentity::of(mock, &identity.key, *index) == *identity
                    })
                })
                .map(|(_, mock)| mock)
                .filter(|mock| mock.sequence.is_some());

            json!({
                "key": identity.label(),
                "method": identity.method(),
                "path": identity.path(),
                "step": counters[identity],
                "total": mock.and_then(|m| m.sequence.as_ref().map(|s| s.len())),
                // The mock's own `source` where it's still loaded, falling back
                // to the file the identity was minted from so a counter left
                // over from a deleted mock still names the file it came from.
                "source": mock
                    .and_then(|m| m.source.clone())
                    .or_else(|| identity.source().map(str::to_string)),
            })
        })
        .collect();

    Json(json!({
        "count": sequences.len(),
        "sequences": sequences,
    }))
}

// ============================================================================
// Scenario (tagged mock group) endpoints
// ============================================================================

/// The `/admin/scenario` payload, shared by the GET and the POST so a caller
/// that just switched scenarios sees exactly what a subsequent GET would say.
///
/// `active_tags` is empty and `filtering` false when no filter is configured,
/// which is also what `POST {"tags": []}` produces — the two ways of saying
/// "everything is matchable" have one representation.
async fn scenario_snapshot(state: &AppState) -> serde_json::Value {
    let active = state.active_tags.read().await;
    let mocks = state.mocks.read().await;

    let mut active_tags: Vec<String> = active.iter().flatten().cloned().collect();
    active_tags.sort();

    let mut known_tags: Vec<String> = mocks
        .values()
        .flatten()
        .flat_map(|mock| mock.tags.iter().cloned())
        .collect::<HashSet<String>>()
        .into_iter()
        .collect();
    known_tags.sort();

    let all_mocks: Vec<&MockConfig> = mocks.values().flatten().collect();
    let matchable = all_mocks
        .iter()
        .filter(|mock| mock.is_active(active.as_ref()))
        .count();

    json!({
        "active_tags": active_tags,
        "filtering": active.is_some(),
        "known_tags": known_tags,
        "matchable_mocks": matchable,
        "total_mocks": all_mocks.len(),
    })
}

/// `GET /admin/scenario` — which scenario tags are active right now.
pub async fn get_scenario(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(scenario_snapshot(&state).await)
}

/// Body of `POST /admin/scenario`.
#[derive(Deserialize, Default)]
pub struct ScenarioRequest {
    /// The tags to activate. Each entry may itself be a comma-separated list,
    /// so `["a,b"]` and `["a", "b"]` mean the same thing — the env var's
    /// syntax works here too. An empty list clears the filter.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `POST /admin/scenario` — replace the active tag set, no restart needed.
///
/// The body is parsed from raw bytes rather than through the `Json` extractor
/// so a plain `curl -d '{"tags": [...]}'` works: curl defaults to a form
/// content type, and rejecting that with a 415 would make the documented
/// one-liner fail for no good reason.
pub async fn set_scenario(State(state): State<AppState>, body: Bytes) -> Response {
    let request: ScenarioRequest = if body.is_empty() {
        ScenarioRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "invalid scenario request",
                        "detail": e.to_string(),
                        "expected": {"tags": ["tag-name"]},
                    })),
                )
                    .into_response();
            }
        }
    };

    {
        let mut active = state.active_tags.write().await;
        *active = parse_active_tags(request.tags);
        match active.as_ref() {
            Some(tags) => {
                let mut names: Vec<&str> = tags.iter().map(String::as_str).collect();
                names.sort();
                info!("Scenario switched: active tags = {}", names.join(", "));
            }
            None => info!("Scenario cleared: all mocks matchable"),
        }
    }

    Json(scenario_snapshot(&state).await).into_response()
}

pub async fn clear_requests(State(state): State<AppState>) -> StatusCode {
    let mut log = state.request_log.write().await;
    log.clear();
    StatusCode::NO_CONTENT
}

/// Resolve the headers and serialize the body of a mock response.
///
/// Custom header names are case-insensitive; invalid names/values are skipped
/// with a warning. `Content-Type: application/json` is added only when the
/// custom headers don't already set a content type. When a non-JSON content
/// type is configured and the response value is a JSON string, the raw string
/// is sent as the body (so XML/CSV/plain-text mocks aren't JSON-quoted).
///
/// Returns the parts rather than a finished [`Response`] so the handler can
/// record exactly what it is about to send, instead of serializing a second
/// copy for the log and trusting the two to agree.
fn build_response_parts(
    response: &serde_json::Value,
    custom_headers: Option<&HashMap<String, String>>,
) -> (HeaderMap, String) {
    let mut header_map = HeaderMap::new();
    if let Some(custom) = custom_headers {
        for (name, value) in custom {
            let parsed_name = axum::http::HeaderName::from_bytes(name.as_bytes());
            let parsed_value = axum::http::HeaderValue::from_str(value);
            match (parsed_name, parsed_value) {
                (Ok(n), Ok(v)) => {
                    header_map.insert(n, v);
                }
                _ => warn!("Skipping invalid response header: {}", name),
            }
        }
    }
    if !header_map.contains_key(CONTENT_TYPE) {
        header_map.insert(
            CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
    }

    let is_json_content_type = header_map
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().contains("json"));

    let body = match response {
        serde_json::Value::String(s) if !is_json_content_type => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };

    (header_map, body)
}

/// Put an already-resolved status, header set, and body on the wire.
fn assemble_response(status: StatusCode, header_map: HeaderMap, body: String) -> Response {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    *res.headers_mut() = header_map;
    res
}

/// Pick the current sequence step and advance the counter.
/// The write lock is held only for the map lookup + clone, never across an await.
async fn advance_sequence(
    counters: &SequenceCounters,
    identity: &MockIdentity,
    steps: &[SequenceStep],
) -> (u16, serde_json::Value, Option<u64>) {
    let mut map = counters.write().await;
    let count = map.entry(identity.clone()).or_insert(0);
    // Clamp so the last step keeps repeating once the sequence is exhausted
    let idx = (*count).min(steps.len() - 1);
    let step = &steps[idx];
    if !step.repeat {
        *count += 1;
    }
    (step.status, step.response.clone(), step.delay_ms)
}

#[derive(Deserialize, Default)]
pub struct SequenceResetFilter {
    pub path: Option<String>,
}

pub async fn reset_sequences(
    State(state): State<AppState>,
    Query(filter): Query<SequenceResetFilter>,
) -> Json<serde_json::Value> {
    let mut counters = state.sequence_counters.write().await;
    let removed = match filter.path {
        Some(ref p) => {
            let before = counters.len();
            counters.retain(|identity, _| identity.path() != p);
            before - counters.len()
        }
        None => {
            let n = counters.len();
            counters.clear();
            n
        }
    };
    info!("Reset {} sequence counter(s)", removed);
    Json(json!({ "reset": removed }))
}

pub async fn admin_dashboard() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BodyMatcher, DelayConfig, HeaderMatcher, HeaderPattern, HeaderValue, JsonBodyMatcher,
        MockConfig, QueryParamMatcher, QueryParamPattern, QueryParamValue,
    };
    use axum::http::Method;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn create_test_state() -> AppState {
        let mut mocks = HashMap::new();

        mocks.insert(
            "GET:/users".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users".to_string(),
                status: 200,
                response: json!({"users": [{"id": 1, "name": "Alice"}]}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        mocks.insert(
            "POST:/login".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/login".to_string(),
                status: 201,
                response: json!({"token": "test-token"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        mocks.insert(
            "DELETE:/users/123".to_string(),
            vec![MockConfig {
                method: "DELETE".to_string(),
                path: "/users/123".to_string(),
                status: 204,
                response: json!(null),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    fn create_empty_state() -> AppState {
        AppState::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())))
    }

    #[tokio::test]
    async fn test_handle_request_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_request_not_found() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/nonexistent".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_post_method() {
        let state = create_test_state();
        let method = Method::POST;
        let uri = "/login".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_handle_request_delete_method() {
        let state = create_test_state();
        let method = Method::DELETE;
        let uri = "/users/123".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_handle_request_wrong_method() {
        let state = create_test_state();
        let method = Method::PUT;
        let uri = "/users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_request_case_sensitive_path() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/Users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = create_test_state();
        let response = health_check(State(state)).await;

        assert_eq!(response.0["status"], "healthy");
        assert_eq!(response.0["mocks_loaded"], 3);
        assert_eq!(response.0["service"], "mimic");
    }

    #[tokio::test]
    async fn test_health_check_empty_state() {
        let state = create_empty_state();
        let response = health_check(State(state)).await;

        assert_eq!(response.0["status"], "healthy");
        assert_eq!(response.0["mocks_loaded"], 0);
    }

    #[tokio::test]
    async fn test_handle_request_response_body() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/users".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

        assert_eq!(parts.status, StatusCode::OK);
        assert_eq!(json["users"][0]["name"], "Alice");
    }

    #[tokio::test]
    async fn test_handle_request_not_found_response_body() {
        let state = create_test_state();
        let method = Method::GET;
        let uri = "/nonexistent".parse().unwrap();
        let headers = HeaderMap::new();

        let response = handle_request(method, uri, headers, State(state), Body::empty()).await;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

        assert_eq!(parts.status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "mock not found");
        assert_eq!(json["method"], "GET");
        assert_eq!(json["path"], "/nonexistent");
    }

    // =========================================================================
    // Query Parameter Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_query_param_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/search".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/search".to_string(),
                status: 200,
                response: json!({"results": []}),
                consume_body: false,
                query_params: Some(QueryParamMatcher {
                    params: HashMap::from([(
                        "q".to_string(),
                        QueryParamValue::Exact("test".to_string()),
                    )]),
                    strict: false,
                }),
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with correct query param
        let method = Method::GET;
        let uri = "/search?q=test".parse().unwrap();
        let headers = HeaderMap::new();
        let response =
            handle_request(method, uri, headers, State(state.clone()), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with wrong query param
        let uri = "/search?q=wrong".parse().unwrap();
        let headers = HeaderMap::new();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_query_param_regex_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users".to_string(),
                status: 200,
                response: json!({"users": []}),
                consume_body: false,
                query_params: Some(QueryParamMatcher {
                    params: HashMap::from([(
                        "page".to_string(),
                        QueryParamValue::Pattern(QueryParamPattern::Regex("^[0-9]+$".to_string())),
                    )]),
                    strict: false,
                }),
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with numeric page
        let uri = "/users?page=123".parse().unwrap();
        let headers = HeaderMap::new();
        let response = handle_request(
            Method::GET,
            uri,
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with non-numeric page
        let uri = "/users?page=abc".parse().unwrap();
        let headers = HeaderMap::new();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Header Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_header_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/protected".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/protected".to_string(),
                status: 200,
                response: json!({"data": "secret"}),
                consume_body: false,
                query_params: None,
                headers: Some(HeaderMatcher {
                    required: HashMap::from([(
                        "authorization".to_string(),
                        HeaderValue::Exact("Bearer token123".to_string()),
                    )]),
                    forbidden: vec![],
                    strict: false,
                }),
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with correct header
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token123".parse().unwrap());
        let uri = "/protected".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with wrong header
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        let uri = "/protected".parse().unwrap();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_header_prefix_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/api".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/api".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: Some(HeaderMatcher {
                    required: HashMap::from([(
                        "authorization".to_string(),
                        HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
                    )]),
                    forbidden: vec![],
                    strict: false,
                }),
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with any Bearer token
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer anytoken".parse().unwrap());
        let uri = "/api".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with Basic auth
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic abc123".parse().unwrap());
        let uri = "/api".parse().unwrap();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Body Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_json_body_exact_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/login".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/login".to_string(),
                status: 200,
                response: json!({"token": "abc123"}),
                consume_body: true,
                query_params: None,
                headers: None,
                body: Some(BodyMatcher::Json(JsonBodyMatcher {
                    exact: Some(json!({"username": "admin", "password": "secret"})),
                    partial: None,
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with exact body
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"username":"admin","password":"secret"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with different body
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"username":"admin","password":"wrong"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_json_body_partial_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/users".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/users".to_string(),
                status: 201,
                response: json!({"id": 1}),
                consume_body: true,
                query_params: None,
                headers: None,
                body: Some(BodyMatcher::Json(JsonBodyMatcher {
                    exact: None,
                    partial: Some(json!({"name": "Alice"})),
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with partial body (extra fields ignored)
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/users".parse().unwrap();
        let body = Body::from(r#"{"name":"Alice","email":"alice@example.com"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::CREATED);

        // Should not match with wrong name
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/users".parse().unwrap();
        let body = Body::from(r#"{"name":"Bob"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Combined Matching Tests
    // =========================================================================

    #[tokio::test]
    async fn test_combined_matching() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/api/search".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/api/search".to_string(),
                status: 200,
                response: json!({"results": ["item1"]}),
                consume_body: true,
                query_params: Some(QueryParamMatcher {
                    params: HashMap::from([(
                        "type".to_string(),
                        QueryParamValue::Exact("user".to_string()),
                    )]),
                    strict: false,
                }),
                headers: Some(HeaderMatcher {
                    required: HashMap::from([(
                        "authorization".to_string(),
                        HeaderValue::Pattern(HeaderPattern::Prefix("Bearer ".to_string())),
                    )]),
                    forbidden: vec![],
                    strict: false,
                }),
                body: Some(BodyMatcher::Json(JsonBodyMatcher {
                    exact: None,
                    partial: Some(json!({"query": "Alice"})),
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Should match with all criteria met
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/api/search?type=user".parse().unwrap();
        let body = Body::from(r#"{"query":"Alice"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Should not match with wrong query param
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/api/search?type=product".parse().unwrap();
        let body = Body::from(r#"{"query":"Alice"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Response Templating Tests
    // =========================================================================

    #[tokio::test]
    async fn test_template_interpolates_body_query_and_header() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/users".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/users".to_string(),
                status: 201,
                response: json!({
                    "id": 99,
                    "username": "{{body.username}}",
                    "email": "{{body.email}}",
                    "created_by": "{{header.x-actor}}",
                    "welcomed_on_page": "{{query.page}}"
                }),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("x-actor", "admin".parse().unwrap());
        let uri = "/users?page=2".parse().unwrap();
        let body = Body::from(r#"{"username":"alice","email":"alice@example.com"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;

        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 99);
        assert_eq!(json["username"], "alice");
        assert_eq!(json["email"], "alice@example.com");
        assert_eq!(json["created_by"], "admin");
        assert_eq!(json["welcomed_on_page"], "2");
    }

    /// #85, end to end: `+` is a space on the way into templating, so a
    /// response echoing `{{query.q}}` returns what the client meant rather
    /// than the wire encoding of it.
    #[tokio::test]
    async fn test_template_query_param_decodes_plus_as_a_space() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/search".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/search".to_string(),
                status: 200,
                response: json!({"echo": "{{query.q}}"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        for query in ["q=hello+world", "q=hello%20world"] {
            let uri = format!("/search?{}", query).parse().unwrap();
            let response = handle_request(
                Method::GET,
                uri,
                HeaderMap::new(),
                State(state.clone()),
                Body::empty(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::OK);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["echo"], "hello world", "for query '{}'", query);
        }
    }

    /// The reported repro: a form matcher written in decoded form has to
    /// accept the `+`-encoded body a browser or `curl -d` actually sends, and
    /// `{{body.*}}` has to render the decoded value.
    #[tokio::test]
    async fn test_form_body_with_a_plus_encoded_space_matches_and_templates() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/form/login".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/form/login".to_string(),
                status: 200,
                response: json!({"ok": true, "password_seen": "{{body.password}}"}),
                consume_body: true,
                query_params: None,
                headers: None,
                body: Some(BodyMatcher::Form(crate::types::FormBodyMatcher {
                    fields: HashMap::from([
                        ("username".to_string(), "alice".to_string()),
                        ("password".to_string(), "secret pass".to_string()),
                    ]),
                    strict: false,
                })),
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        for raw in [
            "username=alice&password=secret+pass",
            "username=alice&password=secret%20pass",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                "application/x-www-form-urlencoded".parse().unwrap(),
            );
            let response = handle_request(
                Method::POST,
                "/form/login".parse().unwrap(),
                headers,
                State(state.clone()),
                Body::from(raw),
            )
            .await;

            assert_eq!(
                response.status(),
                StatusCode::OK,
                "'{}' should match the form matcher",
                raw
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["password_seen"], "secret pass");
        }
    }

    #[tokio::test]
    async fn test_template_nested_body_field() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/orders".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/orders".to_string(),
                status: 200,
                response: json!({"shipping_city": "{{body.address.city}}"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/orders".parse().unwrap();
        let body = Body::from(r#"{"address":{"city":"Jakarta"}}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["shipping_city"], "Jakarta");
    }

    #[tokio::test]
    async fn test_template_missing_variable_renders_empty_string() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/profile".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/profile".to_string(),
                status: 200,
                response: json!({"nickname": "{{query.nickname}}"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let headers = HeaderMap::new();
        let uri = "/profile".parse().unwrap();
        let response = handle_request(Method::GET, uri, headers, State(state), Body::empty()).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["nickname"], "");
    }

    #[tokio::test]
    async fn test_response_without_templates_is_unaffected() {
        let state = create_test_state();
        let uri = "/users".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json, json!({"users": [{"id": 1, "name": "Alice"}]}));
    }

    #[tokio::test]
    async fn test_template_in_sequence_step_response() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/echo".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/echo".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: Some(vec![SequenceStep {
                    status: 200,
                    response: json!({"echoed": "{{body.message}}"}),
                    delay_ms: None,
                    repeat: true,
                }]),
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/echo".parse().unwrap();
        let body = Body::from(r#"{"message":"hello"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["echoed"], "hello");
    }

    #[tokio::test]
    async fn test_faker_templates_render_fresh_values_per_call() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/random".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/random".to_string(),
                status: 200,
                response: json!({
                    "id": "{{faker.uuid}}",
                    "name": "{{faker.name}}",
                    "age": "{{faker.int min=18 max=99}}",
                    "verified": "{{faker.bool}}"
                }),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut ids = Vec::new();
        for _ in 0..5 {
            let response = handle_request(
                Method::GET,
                "/users/random".parse().unwrap(),
                HeaderMap::new(),
                State(state.clone()),
                Body::empty(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            let age: i64 = json["age"].as_str().unwrap().parse().unwrap();
            assert!((18..=99).contains(&age), "{} out of range", age);
            assert!(matches!(
                json["verified"].as_str().unwrap(),
                "true" | "false"
            ));
            assert!(json["name"].as_str().unwrap().contains(' '));
            ids.push(json["id"].as_str().unwrap().to_string());
        }

        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5, "uuid repeated across calls");
    }

    #[tokio::test]
    async fn test_faker_template_in_sequence_step_response() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/jobs".to_string(),
            vec![MockConfig {
                method: "POST".to_string(),
                path: "/jobs".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: Some(vec![SequenceStep {
                    status: 202,
                    response: json!({"job_id": "{{faker.uuid}}"}),
                    delay_ms: None,
                    repeat: true,
                }]),
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let response = handle_request(
            Method::POST,
            "/jobs".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["job_id"].as_str().unwrap().len(), 36);
    }

    #[tokio::test]
    async fn test_faker_is_never_used_for_matching() {
        // Templating fires only after a mock is selected, so a `{{faker.…}}`
        // expression sitting in a matcher stays a literal string to match on.
        let mut params = HashMap::new();
        params.insert(
            "token".to_string(),
            QueryParamValue::Exact("{{faker.uuid}}".to_string()),
        );
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/guarded".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/guarded".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: Some(QueryParamMatcher {
                    params,
                    strict: false,
                }),
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // A real UUID does not satisfy the literal matcher.
        let response = handle_request(
            Method::GET,
            "/guarded?token=3fa85f64-5717-4562-b3fc-2c963f66afa6"
                .parse()
                .unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // The literal value does.
        let response = handle_request(
            Method::GET,
            "/guarded?token=%7B%7Bfaker.uuid%7D%7D".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    // =========================================================================
    // Path Parameter Tests
    // =========================================================================

    #[tokio::test]
    async fn test_path_param_matches_and_templates_into_response() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/:id".to_string(),
                status: 200,
                response: json!({"id": "{{path.id}}", "name": "Mock User"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
        let uri = "/users/42".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], "42");
        assert_eq!(json["name"], "Mock User");
    }

    #[tokio::test]
    async fn test_path_param_brace_syntax_multiple_params() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "DELETE:/orgs/{org}/repos/{repo}".to_string(),
            vec![MockConfig {
                method: "DELETE".to_string(),
                path: "/orgs/{org}/repos/{repo}".to_string(),
                status: 204,
                response: json!(null),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
        let uri = "/orgs/acme/repos/widgets".parse().unwrap();
        let response = handle_request(
            Method::DELETE,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_exact_path_wins_over_path_param_pattern() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/:id".to_string(),
                status: 200,
                response: json!({"source": "pattern"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );
        mocks.insert(
            "GET:/users/42".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/42".to_string(),
                status: 200,
                response: json!({"source": "exact"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // The exact mock wins for id=42
        let uri = "/users/42".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["source"], "exact");

        // Any other id falls through to the pattern mock
        let uri = "/users/7".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["source"], "pattern");
    }

    #[tokio::test]
    async fn test_path_param_sequence_shared_across_ids() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/items/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/items/:id".to_string(),
                status: 200,
                response: json!({"ok": true}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: Some(vec![
                    SequenceStep {
                        status: 503,
                        response: json!({"error": "unavailable"}),
                        delay_ms: None,
                        repeat: false,
                    },
                    SequenceStep {
                        status: 200,
                        response: json!({"ok": true}),
                        delay_ms: None,
                        repeat: true,
                    },
                ]),
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // First request for id=1 consumes step 0 (503)
        let uri = "/items/1".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // A request for a *different* id shares the same sequence counter
        // (keyed by the mock's declared pattern, not the concrete path), so
        // it now sees step 1 (200), not step 0 again.
        let uri = "/items/2".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_no_path_param_match_returns_404() {
        let mut mocks = HashMap::new();
        mocks.insert(
            "GET:/users/:id".to_string(),
            vec![MockConfig {
                method: "GET".to_string(),
                path: "/users/:id".to_string(),
                status: 200,
                response: json!({"name": "Mock User"}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
            }],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));
        let uri = "/posts/1".parse().unwrap();
        let response = handle_request(
            Method::GET,
            uri,
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // Multiple Mocks per METHOD:PATH Tests
    // =========================================================================

    #[tokio::test]
    async fn test_multiple_mocks_same_path_body_differentiation() {
        // Reproduce the bug: two POST /login mocks differ only by body matcher.
        // Both must be retained and the highest-scoring one must win.
        let mut mocks = HashMap::new();
        mocks.insert(
            "POST:/login".to_string(),
            vec![
                MockConfig {
                    method: "POST".to_string(),
                    path: "/login".to_string(),
                    status: 200,
                    response: json!({"role": "admin"}),
                    consume_body: true,
                    query_params: None,
                    headers: None,
                    body: Some(BodyMatcher::Json(JsonBodyMatcher {
                        exact: None,
                        partial: Some(json!({"role": "admin"})),
                        strict: false,
                    })),
                    delay_ms: None,
                    response_headers: None,
                    source: None,
                    sequence: None,
                    tags: Vec::new(),
                },
                MockConfig {
                    method: "POST".to_string(),
                    path: "/login".to_string(),
                    status: 200,
                    response: json!({"role": "user"}),
                    consume_body: true,
                    query_params: None,
                    headers: None,
                    body: Some(BodyMatcher::Json(JsonBodyMatcher {
                        exact: None,
                        partial: Some(json!({"role": "user"})),
                        strict: false,
                    })),
                    delay_ms: None,
                    response_headers: None,
                    source: None,
                    sequence: None,
                    tags: Vec::new(),
                },
            ],
        );

        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        // Request with admin role should return admin mock
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"role":"admin"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state.clone()), body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (_, resp_body) = response.into_parts();
        let bytes = resp_body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["role"], "admin");

        // Request with user role should return user mock
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let uri = "/login".parse().unwrap();
        let body = Body::from(r#"{"role":"user"}"#);
        let response = handle_request(Method::POST, uri, headers, State(state), body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (_, resp_body) = response.into_parts();
        let bytes = resp_body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["role"], "user");
    }

    // =========================================================================
    // Request History API Tests
    // =========================================================================

    #[tokio::test]
    async fn test_list_requests_empty() {
        let state = create_empty_state();
        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 0);
        assert_eq!(response.0["requests"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_request_recording() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        let requests = response.0["requests"].as_array().unwrap();
        assert_eq!(requests[0]["id"], 1);
        assert_eq!(requests[0]["method"], "GET");
        assert_eq!(requests[0]["path"], "/users");
        assert_eq!(requests[0]["response_status"], 200);
        assert_eq!(requests[0]["matched_mock"], "GET:/users");
    }

    #[tokio::test]
    async fn test_request_recording_not_found() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/nonexistent".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        let requests = response.0["requests"].as_array().unwrap();
        assert_eq!(requests[0]["response_status"], 404);
        assert!(requests[0]["matched_mock"].is_null());
    }

    #[tokio::test]
    async fn test_request_filtering_by_path() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter {
            path: Some("/users".to_string()),
            ..Default::default()
        });
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        assert_eq!(response.0["requests"][0]["path"], "/users");
    }

    #[tokio::test]
    async fn test_request_filtering_by_method() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter {
            method: Some("POST".to_string()),
            ..Default::default()
        });
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        assert_eq!(response.0["requests"][0]["method"], "POST");
    }

    #[tokio::test]
    async fn test_request_filtering_by_status() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::GET,
            "/missing".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter {
            status: Some("404".to_string()),
            ..Default::default()
        });
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 1);
        assert_eq!(response.0["requests"][0]["response_status"], 404);
    }

    #[tokio::test]
    async fn test_clear_requests() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        {
            let log = state.request_log.read().await;
            assert_eq!(log.len(), 1);
        }

        let status = clear_requests(State(state.clone())).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 0);
    }

    #[tokio::test]
    async fn test_request_ids_increment() {
        let state = create_test_state();

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        assert_eq!(response.0["count"], 2);
        assert_eq!(response.0["requests"][0]["id"], 1);
        assert_eq!(response.0["requests"][1]["id"], 2);
    }

    #[tokio::test]
    async fn test_sensitive_headers_redacted() {
        let state = create_test_state();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret-token".parse().unwrap());
        headers.insert("cookie", "session=abc123".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let _ = handle_request(
            Method::GET,
            "/users".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let filter = Query(RequestFilter::default());
        let response = list_requests(State(state), filter).await;
        let requests = response.0["requests"].as_array().unwrap();
        let recorded_headers = requests[0]["headers"].as_object().unwrap();
        assert_eq!(recorded_headers["authorization"], "[REDACTED]");
        assert_eq!(recorded_headers["cookie"], "[REDACTED]");
        assert_eq!(recorded_headers["content-type"], "application/json");
    }

    // =========================================================================
    // Stateful Sequence Tests
    // =========================================================================

    fn sequence_mock(path: &str, steps: Vec<SequenceStep>) -> MockConfig {
        MockConfig {
            method: "GET".to_string(),
            path: path.to_string(),
            status: 200,
            response: json!({"fallback": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: Some(steps),
            tags: Vec::new(),
        }
    }

    fn step(status: u16, body: serde_json::Value) -> SequenceStep {
        SequenceStep {
            status,
            response: body,
            delay_ms: None,
            repeat: false,
        }
    }

    fn sequence_state(path: &str, steps: Vec<SequenceStep>) -> AppState {
        let mocks = HashMap::from([(format!("GET:{}", path), vec![sequence_mock(path, steps)])]);
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    async fn call(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
        let response = handle_request(
            Method::GET,
            path.parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (parts.status, json)
    }

    #[tokio::test]
    async fn test_sequence_steps_consumed_in_order() {
        let state = sequence_state(
            "/seq",
            vec![
                step(200, json!({"n": 1})),
                step(429, json!({"n": 2})),
                step(500, json!({"n": 3})),
            ],
        );

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["n"], 1);

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["n"], 2);

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["n"], 3);
    }

    #[tokio::test]
    async fn test_sequence_exhaustion_last_step_repeats() {
        let state = sequence_state(
            "/seq",
            vec![step(201, json!({"n": 1})), step(500, json!({"n": 2}))],
        );

        let _ = call(&state, "/seq").await;
        let _ = call(&state, "/seq").await;

        // Sequence exhausted: last step keeps repeating even without repeat: true
        for _ in 0..3 {
            let (status, body) = call(&state, "/seq").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body["n"], 2);
        }
    }

    #[tokio::test]
    async fn test_sequence_repeat_step_sticks() {
        let mut repeat_step = step(200, json!({"ok": true}));
        repeat_step.repeat = true;
        let state = sequence_state(
            "/seq",
            vec![
                step(503, json!({"error": "unavailable"})),
                repeat_step,
                step(500, json!({"never": true})),
            ],
        );

        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        // The repeat step pins the counter; the step after it is never served
        for _ in 0..3 {
            let (status, body) = call(&state, "/seq").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["ok"], true);
        }
    }

    #[tokio::test]
    async fn test_empty_sequence_falls_back_to_top_level() {
        let state = sequence_state("/seq", vec![]);

        for _ in 0..2 {
            let (status, body) = call(&state, "/seq").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["fallback"], true);
        }
    }

    #[tokio::test]
    async fn test_sequence_delay_applied() {
        let mut delayed = step(200, json!({"ok": true}));
        delayed.delay_ms = Some(30);
        let state = sequence_state("/seq", vec![delayed]);

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "expected at least 30ms delay, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_sequence_invalid_status_falls_back_to_ok() {
        // 99 is below the valid HTTP status range; from_u16 rejects it
        let state = sequence_state("/seq", vec![step(99, json!({"weird": true}))]);

        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["weird"], true);
    }

    #[tokio::test]
    async fn test_sequence_counters_independent_per_variant() {
        // Two sequenced mocks sharing POST:/login, split by body matcher.
        // Each must advance its own counter (proves the #index key).
        let make_mock = |role: &str, steps: Vec<SequenceStep>| MockConfig {
            method: "POST".to_string(),
            path: "/login".to_string(),
            status: 200,
            response: json!({"fallback": true}),
            consume_body: true,
            query_params: None,
            headers: None,
            body: Some(BodyMatcher::Json(JsonBodyMatcher {
                exact: None,
                partial: Some(json!({"role": role})),
                strict: false,
            })),
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: Some(steps),
            tags: Vec::new(),
        };
        let mocks = HashMap::from([(
            "POST:/login".to_string(),
            vec![
                make_mock(
                    "admin",
                    vec![step(200, json!({"n": 1})), step(201, json!({"n": 2}))],
                ),
                make_mock(
                    "user",
                    vec![step(202, json!({"n": 1})), step(203, json!({"n": 2}))],
                ),
            ],
        )]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let post = |state: AppState, role: &'static str| async move {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "application/json".parse().unwrap());
            let body = Body::from(format!(r#"{{"role":"{}"}}"#, role));
            let response = handle_request(
                Method::POST,
                "/login".parse().unwrap(),
                headers,
                State(state),
                body,
            )
            .await;
            response.status()
        };

        // Interleave: each variant advances independently
        assert_eq!(post(state.clone(), "admin").await, StatusCode::OK);
        assert_eq!(post(state.clone(), "user").await, StatusCode::ACCEPTED);
        assert_eq!(post(state.clone(), "admin").await, StatusCode::CREATED);
        let status = post(state.clone(), "user").await;
        assert_eq!(status, StatusCode::NON_AUTHORITATIVE_INFORMATION);
    }

    #[tokio::test]
    async fn test_reset_sequences_all() {
        let state = sequence_state(
            "/seq",
            vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))],
        );

        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);

        let filter = SequenceResetFilter::default();
        let response = reset_sequences(State(state.clone()), Query(filter)).await;
        assert_eq!(response.0["reset"], 1);

        // Sequence starts over from step 1
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_reset_sequences_by_path() {
        let mocks = HashMap::from([
            (
                "GET:/a".to_string(),
                vec![sequence_mock(
                    "/a",
                    vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))],
                )],
            ),
            (
                "GET:/b".to_string(),
                vec![sequence_mock(
                    "/b",
                    vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))],
                )],
            ),
        ]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let _ = call(&state, "/a").await;
        let _ = call(&state, "/b").await;

        let filter = SequenceResetFilter {
            path: Some("/a".to_string()),
        };
        let response = reset_sequences(State(state.clone()), Query(filter)).await;
        assert_eq!(response.0["reset"], 1);

        // /a restarts from step 1, /b continues from step 2
        let (status, _) = call(&state, "/a").await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(&state, "/b").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sequence_thread_safety() {
        let mut repeat_step = step(200, json!({"ok": true}));
        repeat_step.repeat = true;
        let state = sequence_state(
            "/seq",
            vec![
                step(201, json!({"n": 1})),
                step(202, json!({"n": 2})),
                repeat_step,
            ],
        );

        let mut handles = Vec::new();
        for _ in 0..20 {
            let state = state.clone();
            handles.push(tokio::spawn(async move { call(&state, "/seq").await.0 }));
        }

        let mut counts: HashMap<u16, usize> = HashMap::new();
        for handle in handles {
            let status = handle.await.unwrap();
            *counts.entry(status.as_u16()).or_insert(0) += 1;
        }

        // Each non-repeat step is consumed exactly once, regardless of interleaving
        assert_eq!(counts.get(&201), Some(&1));
        assert_eq!(counts.get(&202), Some(&1));
        assert_eq!(counts.get(&200), Some(&18));
    }

    #[tokio::test]
    async fn test_sequence_counter_survives_hot_reload() {
        let steps = vec![step(201, json!({"n": 1})), step(200, json!({"n": 2}))];
        let state = sequence_state("/seq", steps.clone());

        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);

        // Simulate the hot-reload task wholesale-replacing the mock map
        let fresh = HashMap::from([("GET:/seq".to_string(), vec![sequence_mock("/seq", steps)])]);
        *state.mocks.write().await = fresh;

        // Counter survives: next call serves step 2
        let (status, body) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["n"], 2);
    }

    // =========================================================================
    // Response Delay Tests
    // =========================================================================

    fn delayed_state(path: &str, delay: DelayConfig) -> AppState {
        let mut mock = sequence_mock(path, vec![]);
        mock.sequence = None;
        mock.delay_ms = Some(delay);
        let mocks = HashMap::from([(format!("GET:{}", path), vec![mock])]);
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    #[tokio::test]
    async fn test_mock_level_delay_applied() {
        let state = delayed_state("/slow", DelayConfig::Fixed(30));

        let start = std::time::Instant::now();
        let (status, body) = call(&state, "/slow").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["fallback"], true);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "expected at least 30ms delay, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_range_delay_applied() {
        let state = delayed_state("/flaky", DelayConfig::Range { min: 20, max: 40 });

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/flaky").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(20),
            "expected at least 20ms delay, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_step_delay_overrides_mock_delay() {
        let mut step_with_delay = step(200, json!({"ok": true}));
        step_with_delay.delay_ms = Some(10);
        let mut mock = sequence_mock("/seq", vec![step_with_delay]);
        mock.delay_ms = Some(DelayConfig::Fixed(5000));
        let mocks = HashMap::from([("GET:/seq".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::OK);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(10),
            "expected at least the step's 10ms delay, got {:?}",
            elapsed
        );
        assert!(
            elapsed < std::time::Duration::from_millis(5000),
            "mock-level 5000ms delay should have been overridden, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_mock_delay_applies_to_step_without_own_delay() {
        let mut mock = sequence_mock("/seq", vec![step(201, json!({"n": 1}))]);
        mock.delay_ms = Some(DelayConfig::Fixed(30));
        let mocks = HashMap::from([("GET:/seq".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let start = std::time::Instant::now();
        let (status, _) = call(&state, "/seq").await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "expected the mock-level 30ms delay, got {:?}",
            start.elapsed()
        );
    }

    // =========================================================================
    // Custom Response Header Tests
    // =========================================================================

    async fn call_raw(state: &AppState, path: &str) -> (StatusCode, HeaderMap, String) {
        let response = handle_request(
            Method::GET,
            path.parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        (parts.status, parts.headers, text)
    }

    fn headers_state(
        path: &str,
        response: serde_json::Value,
        headers: &[(&str, &str)],
    ) -> AppState {
        let mut mock = sequence_mock(path, vec![]);
        mock.sequence = None;
        mock.response = response;
        mock.response_headers = Some(
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        let mocks = HashMap::from([(format!("GET:{}", path), vec![mock])]);
        AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
    }

    #[tokio::test]
    async fn test_custom_response_headers_present() {
        let state = headers_state(
            "/data",
            json!({"ok": true}),
            &[
                ("X-Custom-Header", "my-value"),
                ("Cache-Control", "no-cache"),
            ],
        );

        let (status, headers, body) = call_raw(&state, "/data").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("x-custom-header").unwrap(), "my-value");
        assert_eq!(headers.get("cache-control").unwrap(), "no-cache");
        // Default Content-Type still applied when not overridden
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(body, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn test_content_type_override_not_doubled() {
        let state = headers_state(
            "/data.xml",
            json!({"ignored": true}),
            &[("Content-Type", "application/xml; charset=utf-8")],
        );

        let (_, headers, _) = call_raw(&state, "/data.xml").await;
        let values: Vec<_> = headers.get_all("content-type").iter().collect();
        assert_eq!(values.len(), 1, "content-type must not be duplicated");
        assert_eq!(values[0], "application/xml; charset=utf-8");
    }

    #[tokio::test]
    async fn test_content_type_override_case_insensitive() {
        // Lowercase key in the config must still suppress the JSON default
        let state = headers_state("/text", json!("hello"), &[("content-type", "text/plain")]);

        let (_, headers, _) = call_raw(&state, "/text").await;
        let values: Vec<_> = headers.get_all("content-type").iter().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], "text/plain");
    }

    #[tokio::test]
    async fn test_non_json_content_type_sends_raw_string_body() {
        let state = headers_state(
            "/data.xml",
            json!("<users><user id=\"1\"/></users>"),
            &[("Content-Type", "application/xml; charset=utf-8")],
        );

        let (status, _, body) = call_raw(&state, "/data.xml").await;
        assert_eq!(status, StatusCode::OK);
        // Raw XML, not a JSON-quoted string
        assert_eq!(body, r#"<users><user id="1"/></users>"#);
    }

    #[tokio::test]
    async fn test_string_response_without_custom_headers_stays_json() {
        let mut mock = sequence_mock("/greeting", vec![]);
        mock.sequence = None;
        mock.response = json!("hello");
        let mocks = HashMap::from([("GET:/greeting".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let (_, headers, body) = call_raw(&state, "/greeting").await;
        // Backward compat: JSON-quoted string with the JSON content type
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(body, r#""hello""#);
    }

    #[tokio::test]
    async fn test_invalid_header_skipped_gracefully() {
        let state = headers_state(
            "/data",
            json!({"ok": true}),
            &[("bad header name", "x"), ("X-Valid", "yes")],
        );

        let (status, headers, _) = call_raw(&state, "/data").await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("bad header name").is_none());
        assert_eq!(headers.get("x-valid").unwrap(), "yes");
    }

    #[tokio::test]
    async fn test_cors_and_location_headers() {
        let state = headers_state(
            "/resources",
            json!({"id": 99}),
            &[
                ("Access-Control-Allow-Origin", "*"),
                ("Location", "/resources/99"),
            ],
        );

        let (_, headers, _) = call_raw(&state, "/resources").await;
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
        assert_eq!(headers.get("location").unwrap(), "/resources/99");
    }

    #[tokio::test]
    async fn test_response_headers_apply_to_sequence_steps() {
        let mut mock = sequence_mock("/seq", vec![step(429, json!({"error": "rate limited"}))]);
        mock.response_headers = Some(HashMap::from([(
            "Retry-After".to_string(),
            "60".to_string(),
        )]));
        let mocks = HashMap::from([("GET:/seq".to_string(), vec![mock])]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let (status, headers, _) = call_raw(&state, "/seq").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(headers.get("retry-after").unwrap(), "60");
    }
    #[tokio::test]
    async fn test_oversized_body_rejected_with_413() {
        let state = create_test_state();
        let max = max_body_size();

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());

        let response = handle_request(
            Method::POST,
            "/echo".parse().unwrap(),
            headers,
            State(state),
            Body::from(vec![b'x'; max + 1]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "payload too large");
        assert_eq!(json["max_body_size"], max);
    }

    #[tokio::test]
    async fn test_body_at_the_limit_is_still_accepted() {
        let state = create_test_state();
        let max = max_body_size();

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());

        let response = handle_request(
            Method::POST,
            "/echo".parse().unwrap(),
            headers,
            State(state),
            Body::from(vec![b'x'; max]),
        )
        .await;

        // No mock matches /echo, but the body itself was accepted rather than
        // rejected as oversized.
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_oversized_streaming_body_is_not_fully_buffered() {
        let state = create_test_state();
        let max = max_body_size();
        let chunk_size = 64 * 1024;

        // A chunked body with no Content-Length that would keep producing far
        // more than the limit. `Limited` must stop polling it once the cap is
        // crossed, so `delivered` never approaches the full (10x limit) size.
        let delivered = Arc::new(AtomicU64::new(0));
        let body = Body::new(EndlessBody {
            delivered: delivered.clone(),
            chunk_size,
            total: max * 10,
        });

        let response = handle_request(
            Method::POST,
            "/echo".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            body,
        )
        .await;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let read = delivered.load(Ordering::Relaxed) as usize;
        assert!(
            read <= max + chunk_size,
            "read {} bytes for a {}-byte limit; the body was buffered past the cap",
            read,
            max
        );
    }

    /// A Content-Length-less body that yields `chunk_size` chunks until
    /// `total` bytes have been handed out, counting everything it emits.
    struct EndlessBody {
        delivered: Arc<AtomicU64>,
        chunk_size: usize,
        total: usize,
    }

    impl http_body::Body for EndlessBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            if self.delivered.load(Ordering::Relaxed) as usize >= self.total {
                return std::task::Poll::Ready(None);
            }
            self.delivered
                .fetch_add(self.chunk_size as u64, Ordering::Relaxed);
            std::task::Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from(vec![
                    b'x';
                    self.chunk_size
                ])))))
        }
    }

    // ------------------------------------------------------------------
    // Per-mock body consumption (#52)
    // ------------------------------------------------------------------

    fn body_mock(path: &str, consume_body: bool) -> MockConfig {
        MockConfig {
            method: "POST".to_string(),
            path: path.to_string(),
            status: 202,
            response: json!({"queued": true}),
            consume_body,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: Vec::new(),
        }
    }

    async fn post_and_read_recorded_body(
        state: &AppState,
        path: &str,
        payload: &'static str,
    ) -> Option<String> {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());

        let _ = handle_request(
            Method::POST,
            path.parse().unwrap(),
            headers,
            State(state.clone()),
            Body::from(payload),
        )
        .await;

        let response = list_requests(State(state.clone()), Query(RequestFilter::default())).await;
        let requests = response.0["requests"].as_array().unwrap();
        requests
            .last()
            .and_then(|r| r["body"].as_str())
            .map(|s| s.to_string())
    }

    fn state_with(mocks: Vec<MockConfig>) -> AppState {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for m in mocks {
            map.entry(crate::types::create_mock_key(&m.method, &m.path))
                .or_default()
                .push(m);
        }
        AppState::new(Arc::new(tokio::sync::RwLock::new(map)))
    }

    #[tokio::test]
    async fn test_consume_body_false_skips_reading_the_body() {
        let state = state_with(vec![body_mock("/trigger-job", false)]);
        let recorded = post_and_read_recorded_body(&state, "/trigger-job", "some=body").await;
        assert_eq!(
            recorded, None,
            "consume_body: false must leave the body unread"
        );
    }

    #[tokio::test]
    async fn test_consume_body_true_reads_the_body() {
        let state = state_with(vec![body_mock("/upload", true)]);
        let recorded = post_and_read_recorded_body(&state, "/upload", "file=contents").await;
        assert_eq!(recorded.as_deref(), Some("file=contents"));
    }

    #[tokio::test]
    async fn test_body_matcher_implies_consumption_without_consume_body() {
        let mut mock = body_mock("/match-me", false);
        mock.body = Some(BodyMatcher::Any);
        let state = state_with(vec![mock]);

        let recorded = post_and_read_recorded_body(&state, "/match-me", "payload").await;
        assert_eq!(recorded.as_deref(), Some("payload"));
    }

    #[tokio::test]
    async fn test_body_template_implies_consumption_without_consume_body() {
        let mut mock = body_mock("/echo-body", false);
        mock.response = json!({"echoed": "{{body.field}}"});
        let state = state_with(vec![mock]);

        let recorded = post_and_read_recorded_body(&state, "/echo-body", "field=value").await;
        assert_eq!(recorded.as_deref(), Some("field=value"));
    }

    #[tokio::test]
    async fn test_another_mocks_body_matcher_does_not_force_consumption() {
        // The regression this fixes: `needs_body_matching` used to be global,
        // so an unrelated mock's body matcher made every request buffer.
        let mut unrelated = body_mock("/has-matcher", false);
        unrelated.path = "/has-matcher".to_string();
        unrelated.body = Some(BodyMatcher::Any);

        let state = state_with(vec![unrelated, body_mock("/trigger-job", false)]);

        let recorded = post_and_read_recorded_body(&state, "/trigger-job", "some=body").await;
        assert_eq!(recorded, None);
    }

    #[tokio::test]
    async fn test_unmatched_request_still_records_its_body() {
        // No mock covers this path, so the request 404s either way — capture
        // the body so the request log can show what was actually sent.
        let state = state_with(vec![body_mock("/trigger-job", false)]);
        let recorded = post_and_read_recorded_body(&state, "/nowhere", "debug=me").await;
        assert_eq!(recorded.as_deref(), Some("debug=me"));
    }

    // ========================================================================
    // Admin dashboard v2: /admin/mocks, /admin/sequences, richer records
    // ========================================================================

    /// A mock with no matchers, loaded from `source`.
    fn dash_mock(method: &str, path: &str, source: Option<&str>) -> MockConfig {
        MockConfig {
            method: method.to_string(),
            path: path.to_string(),
            status: 200,
            response: json!({"ok": true}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: source.map(|s| s.to_string()),
            sequence: None,
            tags: Vec::new(),
        }
    }

    fn dash_state(mocks: Vec<MockConfig>) -> AppState {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for mock in mocks {
            let key = crate::types::create_mock_key(&mock.method, &mock.path);
            map.entry(key).or_default().push(mock);
        }
        AppState::new(Arc::new(tokio::sync::RwLock::new(map)))
    }

    async fn send(state: &AppState, method: Method, path: &str) -> Response {
        handle_request(
            method,
            path.parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await
    }

    /// The single recorded request, for tests that make exactly one.
    async fn only_record(state: &AppState) -> RequestRecord {
        let log = state.request_log.read().await;
        assert_eq!(log.len(), 1, "expected exactly one recorded request");
        log[0].clone()
    }

    // ── GET /admin/mocks ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_mocks_reports_every_loaded_mock() {
        let state = dash_state(vec![
            dash_mock("GET", "/users", Some("mocks/get_users.json")),
            dash_mock("POST", "/login", Some("mocks/post_login.json")),
        ]);

        let body = list_mocks(State(state)).await.0;
        assert_eq!(body["count"], 2);

        let mocks = body["mocks"].as_array().unwrap();
        // Sorted by key, so the listing doesn't reshuffle between refreshes
        assert_eq!(mocks[0]["key"], "GET:/users");
        assert_eq!(mocks[0]["method"], "GET");
        assert_eq!(mocks[0]["path"], "/users");
        assert_eq!(mocks[0]["status"], 200);
        assert_eq!(mocks[0]["source"], "mocks/get_users.json");
        assert_eq!(mocks[1]["key"], "POST:/login");
    }

    #[tokio::test]
    async fn test_list_mocks_reports_matchers_and_shape() {
        let mut mock = dash_mock("GET", "/users/:id", Some("mocks/get_user.json"));
        mock.headers = Some(HeaderMatcher {
            required: HashMap::from([(
                "x-api-key".to_string(),
                HeaderValue::Pattern(HeaderPattern::Any),
            )]),
            forbidden: Vec::new(),
            strict: false,
        });
        mock.delay_ms = Some(DelayConfig::Fixed(250));
        mock.response_headers = Some(HashMap::from([("x-trace".to_string(), "on".to_string())]));

        let body = list_mocks(State(dash_state(vec![mock]))).await.0;
        let entry = &body["mocks"][0];

        assert_eq!(entry["has_path_params"], true);
        assert_eq!(entry["matchers"]["headers"], true);
        assert_eq!(entry["matchers"]["query_params"], false);
        assert_eq!(entry["matchers"]["body"], false);
        assert_eq!(entry["delay_ms"], 250);
        assert_eq!(entry["response_headers"], 1);
        assert_eq!(entry["sequence_steps"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_list_mocks_includes_the_full_config() {
        // The Mocks tab expands a row into the mock file's own JSON, so the
        // whole config has to come down with the summary.
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);
        let body = list_mocks(State(state)).await.0;

        let config = &body["mocks"][0]["config"];
        assert_eq!(config["method"], "GET");
        assert_eq!(config["path"], "/users");
        assert_eq!(config["response"]["ok"], true);
    }

    #[tokio::test]
    async fn test_list_mocks_renders_a_mock_without_a_source() {
        // A mock built in-process has no file to link to; the field is absent
        // rather than an empty string the UI would have to special-case.
        let state = dash_state(vec![dash_mock("GET", "/users", None)]);
        let body = list_mocks(State(state)).await.0;
        assert_eq!(body["mocks"][0]["source"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_list_mocks_counts_hits() {
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);

        for _ in 0..3 {
            let _ = send(&state, Method::GET, "/users").await;
        }
        let _ = send(&state, Method::GET, "/nowhere").await;

        let body = list_mocks(State(state)).await.0;
        assert_eq!(body["mocks"][0]["hits"], 3);
    }

    #[tokio::test]
    async fn test_list_mocks_counts_hits_per_mock_not_per_path() {
        // Two mocks share GET:/users; only the one that actually served the
        // request should show a hit.
        let mut picky = dash_mock("GET", "/users", Some("mocks/picky.json"));
        picky.headers = Some(HeaderMatcher {
            required: HashMap::from([(
                "x-api-key".to_string(),
                HeaderValue::Pattern(HeaderPattern::Any),
            )]),
            forbidden: Vec::new(),
            strict: false,
        });
        let state = dash_state(vec![
            picky,
            dash_mock("GET", "/users", Some("mocks/plain.json")),
        ]);

        let _ = send(&state, Method::GET, "/users").await;

        let body = list_mocks(State(state)).await.0;
        let mocks = body["mocks"].as_array().unwrap();
        let plain = mocks
            .iter()
            .find(|m| m["source"] == "mocks/plain.json")
            .unwrap();
        let picky = mocks
            .iter()
            .find(|m| m["source"] == "mocks/picky.json")
            .unwrap();
        assert_eq!(plain["hits"], 1);
        assert_eq!(picky["hits"], 0);
    }

    #[tokio::test]
    async fn test_list_mocks_reflects_a_hot_reload() {
        // Hot reload swaps the map behind the same lock; the endpoint reads
        // through rather than caching, so a reload is visible immediately.
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);
        assert_eq!(list_mocks(State(state.clone())).await.0["count"], 1);

        {
            let mut mocks = state.mocks.write().await;
            mocks.insert(
                "POST:/login".to_string(),
                vec![dash_mock("POST", "/login", Some("mocks/b.json"))],
            );
        }

        let body = list_mocks(State(state)).await.0;
        assert_eq!(body["count"], 2);
    }

    #[tokio::test]
    async fn test_list_mocks_on_an_empty_store() {
        let state = dash_state(vec![]);
        let body = list_mocks(State(state)).await.0;
        assert_eq!(body["count"], 0);
        assert_eq!(body["mocks"].as_array().unwrap().len(), 0);
    }

    // ── GET /admin/sequences ────────────────────────────────────────────

    fn dash_sequence_mock() -> MockConfig {
        let mut mock = dash_mock("POST", "/submit", Some("mocks/post_submit.json"));
        mock.sequence = Some(vec![
            SequenceStep {
                status: 202,
                response: json!({"state": "pending"}),
                delay_ms: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"state": "done"}),
                delay_ms: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"state": "done"}),
                delay_ms: None,
                repeat: true,
            },
        ]);
        mock
    }

    #[tokio::test]
    async fn test_list_sequences_reports_the_current_step() {
        let state = dash_state(vec![dash_sequence_mock()]);

        let empty = list_sequences(State(state.clone())).await.0;
        assert_eq!(empty["count"], 0, "no counter exists until a request lands");

        let _ = send(&state, Method::POST, "/submit").await;
        let _ = send(&state, Method::POST, "/submit").await;

        let body = list_sequences(State(state)).await.0;
        assert_eq!(body["count"], 1);
        let seq = &body["sequences"][0];
        // The counter is named after the file it belongs to, not its position
        // in a bucket hot reload rebuilds every two seconds.
        assert_eq!(seq["key"], "POST:/submit @ mocks/post_submit.json");
        assert_eq!(seq["method"], "POST");
        assert_eq!(seq["path"], "/submit");
        assert_eq!(seq["step"], 2);
        assert_eq!(seq["total"], 3);
        assert_eq!(seq["source"], "mocks/post_submit.json");
    }

    #[tokio::test]
    async fn test_list_sequences_after_a_reset() {
        let state = dash_state(vec![dash_sequence_mock()]);
        let _ = send(&state, Method::POST, "/submit").await;

        let filter = Query(SequenceResetFilter {
            path: Some("/submit".to_string()),
        });
        let reset = reset_sequences(State(state.clone()), filter).await.0;
        assert_eq!(reset["reset"], 1);

        let body = list_sequences(State(state)).await.0;
        assert_eq!(body["count"], 0);
    }

    #[tokio::test]
    async fn test_list_sequences_survives_a_counter_with_no_mock() {
        // Hot reload can delete the mock a live counter belongs to. The panel
        // must still render the counter rather than blow up looking for it.
        let state = dash_state(vec![]);
        {
            let mut counters = state.sequence_counters.write().await;
            counters.insert(
                MockIdentity {
                    key: "POST:/gone".to_string(),
                    origin: crate::types::MockOrigin::Source("mocks/gone.json".to_string()),
                },
                4,
            );
        }

        let body = list_sequences(State(state)).await.0;
        assert_eq!(body["count"], 1);
        assert_eq!(body["sequences"][0]["step"], 4);
        assert_eq!(body["sequences"][0]["path"], "/gone");
        assert_eq!(body["sequences"][0]["total"], serde_json::Value::Null);
    }

    // ── request record: response, score, path params, explanation ───────

    #[tokio::test]
    async fn test_record_captures_the_response_that_was_served() {
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/a.json"))]);
        let response = send(&state, Method::GET, "/users").await;

        let sent = response.into_body().collect().await.unwrap().to_bytes();
        let record = only_record(&state).await;

        assert_eq!(record.response_status, 200);
        assert_eq!(
            record.response_body.as_deref(),
            Some(String::from_utf8(sent.to_vec()).unwrap().as_str()),
            "the recorded body must be the bytes the client got"
        );
        assert_eq!(
            record
                .response_headers
                .get("content-type")
                .map(String::as_str),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn test_record_captures_score_and_explanation_on_a_match() {
        let state = dash_state(vec![dash_mock(
            "GET",
            "/users",
            Some("mocks/get_users.json"),
        )]);
        let _ = send(&state, Method::GET, "/users").await;

        let record = only_record(&state).await;
        assert_eq!(record.match_score, Some(1000));
        let explanation = record.match_explanation.unwrap();
        assert!(
            explanation.contains("matched mocks/get_users.json"),
            "{}",
            explanation
        );
        assert!(explanation.contains("score 1000"), "{}", explanation);
    }

    #[tokio::test]
    async fn test_record_captures_path_params() {
        let state = dash_state(vec![dash_mock("GET", "/users/:id", Some("mocks/u.json"))]);
        let _ = send(&state, Method::GET, "/users/42").await;

        let record = only_record(&state).await;
        assert_eq!(record.path_params.get("id").map(String::as_str), Some("42"));
        assert_eq!(record.match_score, Some(900));
    }

    #[tokio::test]
    async fn test_record_explains_an_unmatched_request() {
        let mut mock = dash_mock("GET", "/users", Some("mocks/get_users.json"));
        mock.headers = Some(HeaderMatcher {
            required: HashMap::from([(
                "x-api-key".to_string(),
                HeaderValue::Pattern(HeaderPattern::Any),
            )]),
            forbidden: Vec::new(),
            strict: false,
        });
        let state = dash_state(vec![mock]);

        let _ = send(&state, Method::GET, "/users").await;

        let record = only_record(&state).await;
        assert_eq!(record.response_status, 404);
        assert!(record.matched_mock.is_none());
        assert!(record.match_score.is_none());
        let explanation = record.match_explanation.unwrap();
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

    #[tokio::test]
    async fn test_record_of_a_404_keeps_the_error_body() {
        let state = dash_state(vec![]);
        let _ = send(&state, Method::GET, "/nowhere").await;

        let record = only_record(&state).await;
        let body: serde_json::Value =
            serde_json::from_str(record.response_body.as_deref().unwrap()).unwrap();
        assert_eq!(body["error"], "mock not found");
        assert_eq!(body["path"], "/nowhere");
    }

    // ── redaction on the new output surfaces ────────────────────────────

    #[tokio::test]
    async fn test_response_headers_redact_set_cookie() {
        // A mock returning a credential must not have it rendered in the
        // dashboard's new Response section.
        let mut mock = dash_mock("POST", "/login", Some("mocks/login.json"));
        mock.response_headers = Some(HashMap::from([
            ("set-cookie".to_string(), "session=super-secret".to_string()),
            ("x-request-id".to_string(), "abc-123".to_string()),
        ]));
        let state = dash_state(vec![mock]);

        let response = send(&state, Method::POST, "/login").await;
        // The client still gets the real cookie — only the log is redacted
        assert_eq!(
            response.headers().get("set-cookie").unwrap(),
            "session=super-secret"
        );

        let record = only_record(&state).await;
        assert_eq!(
            record
                .response_headers
                .get("set-cookie")
                .map(String::as_str),
            Some("[REDACTED]")
        );
        assert_eq!(
            record
                .response_headers
                .get("x-request-id")
                .map(String::as_str),
            Some("abc-123"),
            "ordinary headers are still useful and must survive"
        );

        let serialized = serde_json::to_string(&record).unwrap();
        assert!(
            !serialized.contains("super-secret"),
            "a credential reached the admin API: {}",
            serialized
        );
    }

    #[tokio::test]
    async fn test_response_headers_redact_authorization_and_cookie() {
        for name in ["authorization", "cookie", "Set-Cookie"] {
            let mut mock = dash_mock("GET", "/echo", Some("mocks/echo.json"));
            mock.response_headers =
                Some(HashMap::from([(name.to_string(), "leak-me".to_string())]));
            let state = dash_state(vec![mock]);

            let _ = send(&state, Method::GET, "/echo").await;

            let record = only_record(&state).await;
            let serialized = serde_json::to_string(&record).unwrap();
            assert!(
                !serialized.contains("leak-me"),
                "{} leaked into the log: {}",
                name,
                serialized
            );
        }
    }

    #[tokio::test]
    async fn test_explanation_never_carries_a_request_credential() {
        let mut mock = dash_mock("GET", "/vault", Some("mocks/vault.json"));
        mock.headers = Some(HeaderMatcher {
            required: HashMap::from([(
                "authorization".to_string(),
                HeaderValue::Exact("Bearer expected-secret".to_string()),
            )]),
            forbidden: Vec::new(),
            strict: false,
        });
        let state = dash_state(vec![mock]);

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer sent-secret".parse().unwrap());
        let _ = handle_request(
            Method::GET,
            "/vault".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;

        let record = only_record(&state).await;
        let serialized = serde_json::to_string(&record).unwrap();
        assert!(!serialized.contains("sent-secret"), "{}", serialized);
        assert!(!serialized.contains("expected-secret"), "{}", serialized);
        assert!(record.match_explanation.unwrap().contains("[REDACTED]"));
    }

    // ── filters ─────────────────────────────────────────────────────────

    /// State with one recorded request per (method, path) given.
    async fn state_with_traffic(traffic: &[(Method, &str)]) -> AppState {
        let state = dash_state(vec![
            dash_mock("GET", "/users", Some("mocks/users.json")),
            dash_mock("GET", "/users/active", Some("mocks/active.json")),
            dash_mock("POST", "/login", Some("mocks/login.json")),
        ]);
        for (method, path) in traffic {
            let _ = send(&state, method.clone(), path).await;
        }
        state
    }

    #[tokio::test]
    async fn test_path_filter_matches_a_substring() {
        // The placeholder has always read like a search box. Now it is one.
        let state = state_with_traffic(&[
            (Method::GET, "/users"),
            (Method::GET, "/users/active"),
            (Method::POST, "/login"),
        ])
        .await;

        let filter = Query(RequestFilter {
            path: Some("user".to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 2);
    }

    #[tokio::test]
    async fn test_path_filter_still_matches_an_exact_path() {
        let state = state_with_traffic(&[(Method::GET, "/users"), (Method::POST, "/login")]).await;

        let filter = Query(RequestFilter {
            path: Some("/login".to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 1);
        assert_eq!(body["requests"][0]["path"], "/login");
    }

    #[tokio::test]
    async fn test_status_filter_accepts_a_class() {
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
        let _ = send(&state, Method::GET, "/users").await; // 200
        let _ = send(&state, Method::GET, "/nope").await; // 404
        let _ = send(&state, Method::GET, "/also-nope").await; // 404

        for (class, expected) in [("4xx", 2), ("2xx", 1), ("5xx", 0)] {
            let filter = Query(RequestFilter {
                status: Some(class.to_string()),
                ..Default::default()
            });
            let body = list_requests(State(state.clone()), filter).await.0;
            assert_eq!(body["count"], expected, "status={}", class);
        }
    }

    #[tokio::test]
    async fn test_status_filter_accepts_an_exact_code() {
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
        let _ = send(&state, Method::GET, "/users").await;
        let _ = send(&state, Method::GET, "/nope").await;

        let filter = Query(RequestFilter {
            status: Some("404".to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 1);
        assert_eq!(body["requests"][0]["response_status"], 404);
    }

    #[tokio::test]
    async fn test_unmatched_only_filter() {
        let state = dash_state(vec![dash_mock("GET", "/users", Some("mocks/users.json"))]);
        let _ = send(&state, Method::GET, "/users").await;
        let _ = send(&state, Method::GET, "/nope").await;

        let filter = Query(RequestFilter {
            unmatched_only: Some("true".to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state.clone()), filter).await.0;
        assert_eq!(body["count"], 1);
        assert_eq!(body["requests"][0]["path"], "/nope");

        // An empty value reads as "off" rather than failing the request
        let filter = Query(RequestFilter {
            unmatched_only: Some(String::new()),
            ..Default::default()
        });
        assert_eq!(list_requests(State(state), filter).await.0["count"], 2);
    }

    #[tokio::test]
    async fn test_search_filter_looks_in_the_body() {
        let state = dash_state(vec![]);
        for body in ["needle in here", "nothing to see"] {
            let _ = handle_request(
                Method::POST,
                "/inbox".parse().unwrap(),
                HeaderMap::new(),
                State(state.clone()),
                Body::from(body),
            )
            .await;
        }

        let filter = Query(RequestFilter {
            search: Some("NEEDLE".to_string()), // case-insensitive
            ..Default::default()
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 1);
    }

    #[tokio::test]
    async fn test_search_filter_looks_in_the_headers() {
        let state = dash_state(vec![]);
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant", "acme-corp".parse().unwrap());
        let _ = handle_request(
            Method::GET,
            "/a".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let _ = send(&state, Method::GET, "/b").await;

        let filter = Query(RequestFilter {
            search: Some("acme".to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 1);
        assert_eq!(body["requests"][0]["path"], "/a");
    }

    #[tokio::test]
    async fn test_filters_combine() {
        let state = state_with_traffic(&[
            (Method::GET, "/users"),
            (Method::GET, "/users/active"),
            (Method::POST, "/login"),
        ])
        .await;

        let filter = Query(RequestFilter {
            path: Some("user".to_string()),
            method: Some("GET".to_string()),
            status: Some("2xx".to_string()),
            ..Default::default()
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 2);
    }

    #[tokio::test]
    async fn test_empty_filters_are_ignored() {
        // The dashboard omits empty boxes, but a hand-written curl may not.
        let state = state_with_traffic(&[(Method::GET, "/users"), (Method::POST, "/login")]).await;

        let filter = Query(RequestFilter {
            path: Some(String::new()),
            method: Some(String::new()),
            status: Some(String::new()),
            search: Some("   ".to_string()),
            unmatched_only: None,
        });
        let body = list_requests(State(state), filter).await.0;
        assert_eq!(body["count"], 2);
    }

    #[test]
    fn test_status_matches() {
        assert!(status_matches("404", 404));
        assert!(!status_matches("404", 400));
        assert!(status_matches("4xx", 404));
        assert!(status_matches("4XX", 451));
        assert!(!status_matches("4xx", 500));
        assert!(status_matches("5xx", 503));
        assert!(status_matches("", 200), "an empty filter filters nothing");
        // A filter that can't be understood matches nothing, so a typo shows an
        // empty table rather than a full one that looks unfiltered.
        assert!(!status_matches("nonsense", 200));
        assert!(!status_matches("xx", 200));
    }

    #[test]
    fn test_is_truthy() {
        for yes in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy(yes), "{}", yes);
        }
        for no in ["", "0", "false", "no", "maybe"] {
            assert!(!is_truthy(no), "{}", no);
        }
    }

    // ── bounded log and truncated bodies ────────────────────────────────

    #[test]
    fn test_truncate_body() {
        assert_eq!(truncate_body("short".to_string(), 10), "short");
        assert_eq!(truncate_body("exactly10!".to_string(), 10), "exactly10!");

        let truncated = truncate_body("0123456789abc".to_string(), 10);
        assert_eq!(truncated, format!("0123456789{}", TRUNCATION_MARKER));

        // 0 opts out entirely
        let long = "x".repeat(5000);
        assert_eq!(truncate_body(long.clone(), 0), long);
    }

    #[test]
    fn test_truncate_body_respects_character_boundaries() {
        // Cutting a multi-byte character in half would make the record
        // unserializable; the cut walks back to a boundary instead.
        let body = "héllo wörld".to_string();
        for limit in 1..body.len() {
            let truncated = truncate_body(body.clone(), limit);
            assert!(
                truncated.ends_with(TRUNCATION_MARKER),
                "limit {} should truncate",
                limit
            );
            // Round-trips through JSON, which only accepts valid UTF-8
            assert!(serde_json::to_string(&truncated).is_ok());
        }
    }

    #[test]
    fn test_push_bounded_drops_the_oldest() {
        let mut log: Vec<RequestRecord> = Vec::new();
        for id in 1..=10 {
            let record = RequestRecord {
                id,
                ..Default::default()
            };
            push_bounded(&mut log, record, 3);
        }

        assert_eq!(log.len(), 3);
        assert_eq!(log.iter().map(|r| r.id).collect::<Vec<_>>(), vec![8, 9, 10]);
    }

    #[test]
    fn test_push_bounded_with_no_cap() {
        let mut log: Vec<RequestRecord> = Vec::new();
        for id in 1..=50 {
            push_bounded(
                &mut log,
                RequestRecord {
                    id,
                    ..Default::default()
                },
                0,
            );
        }
        assert_eq!(log.len(), 50);
    }

    #[tokio::test]
    async fn test_a_large_response_body_is_truncated_in_the_log() {
        let mut mock = dash_mock("GET", "/big", Some("mocks/big.json"));
        mock.response = json!({"blob": "x".repeat(200_000)});
        let state = dash_state(vec![mock]);

        let response = send(&state, Method::GET, "/big").await;
        let sent = response.into_body().collect().await.unwrap().to_bytes();
        assert!(sent.len() > 100_000, "the client still gets the whole body");

        let record = only_record(&state).await;
        let stored = record.response_body.unwrap();
        assert!(
            stored.len() < sent.len(),
            "the log kept the whole {}-byte body",
            sent.len()
        );
        assert!(stored.ends_with(TRUNCATION_MARKER));
    }

    // ── /health summary ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_health_reports_the_dashboard_summary() {
        let state = dash_state(vec![
            dash_mock("GET", "/users", Some("mocks/a.json")),
            dash_mock("POST", "/login", Some("mocks/b.json")),
        ]);
        let _ = send(&state, Method::GET, "/users").await;

        let body = health_check(State(state)).await.0;
        assert_eq!(body["status"], "healthy");
        assert_eq!(body["mocks_loaded"], 2);
        assert_eq!(body["mock_count"], 2);
        assert_eq!(body["requests_recorded"], 1);
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body["port"].is_number());
        assert!(body["max_body_size"].is_number());
        assert!(body["uptime_seconds"].is_number());
    }

    #[tokio::test]
    async fn test_health_counts_mocks_behind_a_shared_route() {
        // Two mocks, one route: `mocks_loaded` keeps its old meaning and
        // `mock_count` reports the definitions.
        let state = dash_state(vec![
            dash_mock("GET", "/users", Some("mocks/a.json")),
            dash_mock("GET", "/users", Some("mocks/b.json")),
        ]);
        let body = health_check(State(state)).await.0;
        assert_eq!(body["mocks_loaded"], 1);
        assert_eq!(body["mock_count"], 2);
    }

    // ── the dashboard page itself ───────────────────────────────────────

    #[tokio::test]
    async fn test_dashboard_html_wires_up_the_new_surfaces() {
        let html = admin_dashboard().await.0;
        for needle in [
            "/admin/mocks",
            "/admin/sequences",
            "unmatched_only=true",
            "prefers-color-scheme",
            "data-tab=\"mocks\"",
            "data-tab=\"sequences\"",
        ] {
            assert!(html.contains(needle), "dashboard is missing {}", needle);
        }
    }
}

// ============================================================================
// Scenario endpoint tests (#62)
// ============================================================================

#[cfg(test)]
mod scenario_tests {
    use super::*;
    use crate::types::{MockConfig, SequenceStep};
    use axum::http::Method;
    use http_body_util::BodyExt;

    fn mock(method: &str, path: &str, marker: &str, tags: &[&str]) -> MockConfig {
        MockConfig {
            method: method.to_string(),
            path: path.to_string(),
            status: 200,
            response: json!({"source": marker}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn state_with(mocks: Vec<MockConfig>, active: Option<&[&str]>) -> AppState {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for m in mocks {
            let key = crate::types::create_mock_key(&m.method, &m.path);
            map.entry(key).or_default().push(m);
        }
        AppState::with_active_tags(
            Arc::new(tokio::sync::RwLock::new(map)),
            active.map(|tags| tags.iter().map(|t| t.to_string()).collect()),
        )
    }

    /// Two mocks on one path, told apart by scenario — the setup from the
    /// issue: a happy-path checkout and its 500 counterpart.
    fn checkout_state(active: Option<&[&str]>) -> AppState {
        state_with(
            vec![
                mock("POST", "/checkout", "ok", &["happy-path"]),
                mock("POST", "/checkout", "boom", &["error-scenario"]),
            ],
            active,
        )
    }

    async fn get(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
        request(state, Method::GET, path).await
    }

    async fn request(
        state: &AppState,
        method: Method,
        path: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = handle_request(
            method,
            path.parse().unwrap(),
            HeaderMap::new(),
            State(state.clone()),
            Body::empty(),
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (parts.status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post_scenario(state: &AppState, body: &str) -> (StatusCode, serde_json::Value) {
        let response = set_scenario(State(state.clone()), Bytes::from(body.to_string())).await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (parts.status, serde_json::from_slice(&bytes).unwrap())
    }

    // ── GET /admin/scenario ─────────────────────────────────────────────

    #[tokio::test]
    async fn scenario_defaults_to_no_filtering() {
        let state = checkout_state(None);
        let Json(body) = get_scenario(State(state)).await;

        assert_eq!(body["filtering"], false);
        assert_eq!(body["active_tags"], json!([]));
        assert_eq!(body["known_tags"], json!(["error-scenario", "happy-path"]));
        assert_eq!(body["matchable_mocks"], 2);
        assert_eq!(body["total_mocks"], 2);
    }

    #[tokio::test]
    async fn scenario_reports_the_startup_selection() {
        let state = checkout_state(Some(&["happy-path"]));
        let Json(body) = get_scenario(State(state)).await;

        assert_eq!(body["filtering"], true);
        assert_eq!(body["active_tags"], json!(["happy-path"]));
        assert_eq!(body["matchable_mocks"], 1);
    }

    // ── POST /admin/scenario ────────────────────────────────────────────

    #[tokio::test]
    async fn posting_tags_replaces_the_active_set() {
        let state = checkout_state(Some(&["happy-path"]));

        let (status, body) = post_scenario(&state, r#"{"tags": ["error-scenario"]}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["active_tags"], json!(["error-scenario"]));

        // A follow-up GET agrees with what the POST reported.
        let Json(after) = get_scenario(State(state)).await;
        assert_eq!(after["active_tags"], json!(["error-scenario"]));
    }

    #[tokio::test]
    async fn posting_an_empty_list_clears_the_filter() {
        let state = checkout_state(Some(&["happy-path"]));

        let (status, body) = post_scenario(&state, r#"{"tags": []}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["filtering"], false);
        assert_eq!(body["matchable_mocks"], 2);
    }

    #[tokio::test]
    async fn posting_a_comma_separated_tag_works_like_the_env_var() {
        let state = checkout_state(None);

        let (_, body) = post_scenario(&state, r#"{"tags": ["happy-path, error-scenario"]}"#).await;
        assert_eq!(body["active_tags"], json!(["error-scenario", "happy-path"]));
    }

    #[tokio::test]
    async fn posting_malformed_json_is_a_400_and_leaves_the_scenario_alone() {
        let state = checkout_state(Some(&["happy-path"]));

        let (status, body) = post_scenario(&state, "not json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid scenario request");

        let Json(after) = get_scenario(State(state)).await;
        assert_eq!(after["active_tags"], json!(["happy-path"]));
    }

    // ── End to end: switching scenarios changes what is served ──────────

    #[tokio::test]
    async fn switching_scenarios_switches_the_served_mock_without_a_restart() {
        let state = checkout_state(Some(&["happy-path"]));

        let (status, body) = request(&state, Method::POST, "/checkout").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "ok");

        post_scenario(&state, r#"{"tags": ["error-scenario"]}"#).await;

        let (status, body) = request(&state, Method::POST, "/checkout").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "boom");
    }

    #[tokio::test]
    async fn an_inactive_mock_404s_as_if_it_were_not_loaded() {
        let state = state_with(
            vec![mock("POST", "/checkout", "boom", &["error-scenario"])],
            Some(&["happy-path"]),
        );

        let (status, body) = request(&state, Method::POST, "/checkout").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "mock not found");
        // The 404 body is the same one an unregistered path gets: scenario
        // configuration is not leaked to API clients.
        assert!(body.get("tags").is_none());
        assert!(!body.to_string().contains("error-scenario"));

        // The reason is available to operators through the request log.
        let log = state.request_log.read().await;
        let explanation = log[0].match_explanation.as_deref().unwrap_or_default();
        assert!(explanation.contains("inactive tags"), "{}", explanation);
    }

    #[tokio::test]
    async fn untagged_mocks_are_unaffected_by_any_scenario() {
        let state = state_with(vec![mock("GET", "/users", "plain", &[])], Some(&["chaos"]));

        let (status, body) = get(&state, "/users").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "plain");

        post_scenario(&state, r#"{"tags": ["anything-else"]}"#).await;

        let (status, body) = get(&state, "/users").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "plain");
    }

    // ── Interaction with other state ────────────────────────────────────

    #[tokio::test]
    async fn switching_scenarios_does_not_reset_sequence_counters() {
        let mut sequenced = mock("GET", "/poll", "seq", &["happy-path"]);
        sequenced.sequence = Some(vec![
            SequenceStep {
                status: 200,
                response: json!({"n": 1}),
                delay_ms: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"n": 2}),
                delay_ms: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"n": 3}),
                delay_ms: None,
                repeat: true,
            },
        ]);
        let state = state_with(vec![sequenced], Some(&["happy-path"]));

        assert_eq!(get(&state, "/poll").await.1["n"], 1);
        assert_eq!(get(&state, "/poll").await.1["n"], 2);

        // Switch away (the mock stops matching), then back.
        post_scenario(&state, r#"{"tags": ["error-scenario"]}"#).await;
        assert_eq!(
            get(&state, "/poll").await.0,
            StatusCode::NOT_FOUND,
            "the sequenced mock is inactive while the scenario is switched away"
        );
        post_scenario(&state, r#"{"tags": ["happy-path"]}"#).await;

        // The counter picked up where it left off rather than restarting.
        assert_eq!(get(&state, "/poll").await.1["n"], 3);
    }

    #[tokio::test]
    async fn hit_counts_stay_keyed_to_the_right_mock_across_a_switch() {
        let state = checkout_state(Some(&["error-scenario"]));

        request(&state, Method::POST, "/checkout").await;

        let hits = state.mock_hits.read().await;
        // These mocks are built in-process with no `source`, so their identity
        // falls back to bucket position. Index 1 is the error-scenario mock's
        // real position; filtering must not renumber the mocks around it.
        let at = |index: usize| MockIdentity {
            key: "POST:/checkout".to_string(),
            origin: crate::types::MockOrigin::Position(index),
        };
        assert_eq!(hits.get(&at(1)).copied(), Some(1));
        assert_eq!(hits.get(&at(0)).copied(), None);
    }

    #[tokio::test]
    async fn admin_mocks_reports_tags_and_whether_each_mock_is_active() {
        let state = checkout_state(Some(&["happy-path"]));
        let Json(body) = list_mocks(State(state)).await;

        let entries = body["mocks"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["tags"], json!(["happy-path"]));
        assert_eq!(entries[0]["active"], true);
        assert_eq!(entries[1]["tags"], json!(["error-scenario"]));
        assert_eq!(entries[1]["active"], false);
    }

    // ── MIMIC_ACTIVE_TAGS parsing ───────────────────────────────────────

    #[test]
    fn configured_active_tags_reads_the_env_var() {
        // Serialized by the env var itself: this is the only test that touches
        // MIMIC_ACTIVE_TAGS, so the mutation can't race another test.
        let restore = std::env::var(ACTIVE_TAGS_ENV).ok();

        std::env::set_var(ACTIVE_TAGS_ENV, "happy-path, smoke-test");
        assert_eq!(
            configured_active_tags(),
            Some(
                ["happy-path", "smoke-test"]
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<HashSet<String>>()
            )
        );

        std::env::set_var(ACTIVE_TAGS_ENV, "");
        assert_eq!(configured_active_tags(), None);

        std::env::remove_var(ACTIVE_TAGS_ENV);
        assert_eq!(configured_active_tags(), None);

        if let Some(value) = restore {
            std::env::set_var(ACTIVE_TAGS_ENV, value);
        }
    }
}
