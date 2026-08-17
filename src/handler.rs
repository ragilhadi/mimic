use crate::cors::{self, CorsConfig};
use crate::matcher::{
    explain_match, explain_no_match, find_matching_mock, is_pattern_path, parse_body,
    parse_headers, parse_query_string, requires_body, route_exists, MatchResult, RequestContext,
};
use crate::template::{render_response, TemplateContext};
use crate::types::{
    is_sensitive_header, is_truthy, parse_active_tags, ActiveTags, MockConfig, MockIdentity,
    MockStore, RequestLog, RequestRecord, SequenceCounters, SequenceStep,
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
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Per-mock hit counts, keyed like the sequence counters — by the mock's
/// stable [`MockIdentity`] — so one mock among several registered for a path
/// can be told from the rest, and keeps its count across a hot reload.
pub type MockHits = Arc<tokio::sync::RwLock<HashMap<MockIdentity, u64>>>;

// ============================================================================
// Reserved Routes
// ============================================================================

/// Environment variable moving the admin API off `/admin`.
pub const ADMIN_PREFIX_ENV: &str = "MIMIC_ADMIN_PREFIX";

/// Environment variable switching the admin API off entirely.
pub const DISABLE_ADMIN_ENV: &str = "MIMIC_DISABLE_ADMIN";

/// Environment variable moving — or, when empty, removing — the health check.
pub const HEALTH_PATH_ENV: &str = "MIMIC_HEALTH_PATH";

/// Default path of the health check endpoint.
pub const DEFAULT_HEALTH_PATH: &str = "/health";

/// Default prefix the admin API is mounted under.
pub const DEFAULT_ADMIN_PREFIX: &str = "/admin";

/// The paths under the admin prefix, paired with the methods they answer.
///
/// This is the list `create_router` registers, and the list a mock is checked
/// against — one array, so a route can't be reserved in the diagnostic and
/// unregistered in the router, or the reverse.
const ADMIN_ROUTES: &[(&str, &[&str])] = &[
    ("/dashboard", &["GET"]),
    ("/requests", &["GET", "DELETE"]),
    ("/mocks", &["GET"]),
    ("/sequences", &["GET"]),
    ("/sequences/reset", &["POST"]),
    ("/scenario", &["GET", "POST"]),
];

/// The method+path pairs Mimic answers itself, and which a mock therefore
/// cannot serve.
///
/// Explicit routes are matched ahead of the fallback by design, so a mock
/// declaring one of these paths loads fine, is reported by `/admin/mocks`, and
/// then never serves a request. Naming the reservations makes that sayable —
/// at load time, and on the dashboard — and makes them movable, for anyone
/// whose API genuinely owns `/health` or `/admin/...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedRoutes {
    /// Where the health check is served, or `None` when it is switched off.
    pub health: Option<String>,
    /// The prefix the admin API is mounted under, or `None` when it is
    /// switched off. Never has a trailing slash.
    pub admin_prefix: Option<String>,
}

impl Default for ReservedRoutes {
    fn default() -> Self {
        Self {
            health: Some(DEFAULT_HEALTH_PATH.to_string()),
            admin_prefix: Some(DEFAULT_ADMIN_PREFIX.to_string()),
        }
    }
}

impl ReservedRoutes {
    /// Read the configuration from the environment.
    ///
    /// Both knobs default to today's behavior, so an existing deployment that
    /// sets neither is routed exactly as it was.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(HEALTH_PATH_ENV).ok(),
            std::env::var(ADMIN_PREFIX_ENV).ok(),
            std::env::var(DISABLE_ADMIN_ENV).ok(),
        )
    }

    /// [`ReservedRoutes::from_env`] with the environment injected, so the
    /// rules are testable without mutating variables other tests share.
    pub fn from_values(
        health_path: Option<String>,
        admin_prefix: Option<String>,
        disable_admin: Option<String>,
    ) -> Self {
        let health = match health_path {
            // An explicitly empty value is the way to say "don't serve it".
            Some(raw) => normalize_route(&raw, HEALTH_PATH_ENV),
            None => Some(DEFAULT_HEALTH_PATH.to_string()),
        };

        let admin = if disable_admin.as_deref().is_some_and(is_truthy) {
            None
        } else {
            match admin_prefix {
                Some(raw) => normalize_route(&raw, ADMIN_PREFIX_ENV),
                None => Some(DEFAULT_ADMIN_PREFIX.to_string()),
            }
        };

        Self {
            health,
            admin_prefix: admin,
        }
    }

    /// Why a mock for `method path` would never be served, or `None` if it
    /// would.
    ///
    /// Deliberately narrow: only the exact method+path pairs Mimic answers are
    /// reserved. `POST /health` and `GET /admin/users` reach the fallback and
    /// are perfectly good mocks.
    pub fn reservation_for(&self, method: &str, path: &str) -> Option<&'static str> {
        if self
            .health
            .as_deref()
            .is_some_and(|health| health == path && method.eq_ignore_ascii_case("GET"))
        {
            return Some("Mimic's health check");
        }

        self.is_admin_endpoint(method, path)
            .then_some("Mimic's admin API")
    }

    /// Whether `method path` is one of the admin API's own endpoints.
    ///
    /// This is the set `MIMIC_ADMIN_TOKEN` guards. The health check is
    /// deliberately outside it: liveness probes call it and carry no
    /// credentials.
    pub fn is_admin_endpoint(&self, method: &str, path: &str) -> bool {
        let Some(prefix) = self.admin_prefix.as_deref() else {
            return false;
        };
        let Some(suffix) = path.strip_prefix(prefix) else {
            return false;
        };
        ADMIN_ROUTES.iter().any(|(route, methods)| {
            *route == suffix
                && methods
                    .iter()
                    .any(|allowed| method.eq_ignore_ascii_case(allowed))
        })
    }
}

// ============================================================================
// Body Redaction and Admin Authentication
// ============================================================================

/// The placeholder a redacted value is stored as. Shared with header
/// redaction so the log reads consistently.
pub const REDACTED: &str = "[REDACTED]";

/// Environment variable listing the body field names to redact.
pub const REDACT_BODY_FIELDS_ENV: &str = "MIMIC_REDACT_BODY_FIELDS";

/// Environment variable switching body storage off entirely.
pub const DISABLE_BODY_LOG_ENV: &str = "MIMIC_DISABLE_BODY_LOG";

/// Environment variable setting the bearer token the admin API requires.
pub const ADMIN_TOKEN_ENV: &str = "MIMIC_ADMIN_TOKEN";

/// Field names redacted from stored bodies when nothing is configured.
///
/// A default rather than an empty list, because the failure mode is silent:
/// nobody discovers that the log kept a password until someone reads the log.
/// `MIMIC_REDACT_BODY_FIELDS=` (empty) restores verbatim storage for anyone
/// who wants it.
pub const DEFAULT_REDACT_BODY_FIELDS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "secret",
    "client_secret",
    "api_key",
    "apikey",
    "private_key",
    "authorization",
];

/// What is kept, and what is scrubbed, from the bodies stored on a log entry.
///
/// Only the *stored* copy is affected. The body sent to the client, the body
/// matching runs against, and the values templating interpolates are all
/// untouched — redaction is a property of the log, not of the mock server's
/// behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyRedaction {
    /// Lowercased field names whose values are replaced. Empty means bodies
    /// are stored verbatim.
    pub fields: HashSet<String>,
    /// Store no bodies at all — request or response.
    pub disabled: bool,
}

impl Default for BodyRedaction {
    fn default() -> Self {
        Self {
            fields: DEFAULT_REDACT_BODY_FIELDS
                .iter()
                .map(|field| field.to_string())
                .collect(),
            disabled: false,
        }
    }
}

impl BodyRedaction {
    /// Read the policy from the environment.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(REDACT_BODY_FIELDS_ENV).ok(),
            std::env::var(DISABLE_BODY_LOG_ENV).ok(),
        )
    }

    /// [`BodyRedaction::from_env`] with the environment injected, so the rules
    /// are testable without mutating variables other tests share.
    pub fn from_values(fields: Option<String>, disable: Option<String>) -> Self {
        let fields = match fields {
            // Set-but-empty is the documented way to say "store verbatim",
            // which is why this doesn't collapse into `unwrap_or_default`.
            Some(raw) => raw
                .split(',')
                .map(|field| field.trim().to_ascii_lowercase())
                .filter(|field| !field.is_empty())
                .collect(),
            None => Self::default().fields,
        };

        Self {
            fields,
            disabled: disable.as_deref().is_some_and(is_truthy),
        }
    }

    /// The stored form of a body, or `None` when bodies aren't stored.
    pub fn apply(&self, body: String, content_type: Option<&str>) -> Option<String> {
        if self.disabled {
            return None;
        }
        if self.fields.is_empty() {
            return Some(body);
        }
        Some(redact_body(&body, content_type, &self.fields))
    }
}

/// Replace the values of `fields` in `body`, leaving everything else alone.
///
/// Best-effort by structure, not by pattern: a body that parses as JSON is
/// walked and rewritten field-wise, including through nested objects and
/// arrays; a urlencoded form body is rewritten field-wise; anything else has
/// no field structure to redact and is returned as it came in. Matching a
/// field name is case-insensitive and exact — `token` does not scrub
/// `tokenizer` — which is why the default list spells out the common variants.
///
/// The original string is returned unchanged when nothing matched, so a body
/// with no secrets in it is never reserialized into different whitespace.
pub fn redact_body(body: &str, content_type: Option<&str>, fields: &HashSet<String>) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) {
        if redact_json(&mut value, fields) {
            return serde_json::to_string(&value).unwrap_or_else(|_| body.to_string());
        }
        return body.to_string();
    }

    let is_form = content_type
        .map(|ct| ct.to_ascii_lowercase())
        .is_some_and(|ct| ct.contains("application/x-www-form-urlencoded"));
    if is_form {
        if let Some(redacted) = redact_form(body, fields) {
            return redacted;
        }
    }

    body.to_string()
}

/// Redact matching fields anywhere in a JSON value. Returns whether anything
/// changed.
///
/// A matching key has its **whole** value replaced, object or array included:
/// `{"token": {"value": "..."}}` must not leak just because the secret is one
/// level further in.
fn redact_json(value: &mut serde_json::Value, fields: &HashSet<String>) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            let mut changed = false;
            for (key, entry) in map.iter_mut() {
                if fields.contains(&key.to_ascii_lowercase()) {
                    *entry = serde_json::Value::String(REDACTED.to_string());
                    changed = true;
                } else {
                    changed |= redact_json(entry, fields);
                }
            }
            changed
        }
        serde_json::Value::Array(items) => {
            // A plain loop rather than `any`: every element has to be visited,
            // and `any` short-circuits on the first hit — which would leave
            // every later element of the array unredacted.
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= redact_json(item, fields);
            }
            changed
        }
        _ => false,
    }
}

/// Redact matching fields in a urlencoded body, or `None` if nothing matched.
///
/// Everything not redacted is passed through byte for byte — this rewrites the
/// log entry, and a body that came back subtly re-encoded would be a worse
/// record of what the client sent than the original.
fn redact_form(body: &str, fields: &HashSet<String>) -> Option<String> {
    let mut changed = false;
    let rewritten: Vec<String> = body
        .split('&')
        .map(|pair| {
            let Some((key, _)) = pair.split_once('=') else {
                return pair.to_string();
            };
            // Compared decoded, the same form the matcher and the templater
            // see the field name in.
            if fields.contains(&crate::matcher::decode_component(key).to_ascii_lowercase()) {
                changed = true;
                format!("{}={}", key, REDACTED)
            } else {
                pair.to_string()
            }
        })
        .collect();

    changed.then(|| rewritten.join("&"))
}

/// The bearer token the admin API requires, read from `MIMIC_ADMIN_TOKEN`.
///
/// Unset — the default — leaves the admin API open exactly as it has always
/// been, so nothing breaks for an existing user who hasn't asked for auth.
pub fn configured_admin_token() -> Option<String> {
    std::env::var(ADMIN_TOKEN_ENV)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// 401 returned when the admin API is protected and the caller didn't present
/// the token. Same shape as the other error bodies Mimic returns.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "unauthorized",
            "detail": format!(
                "the admin API requires an 'Authorization: Bearer <token>' header \
                 matching {}",
                ADMIN_TOKEN_ENV
            ),
        })),
    )
        .into_response()
}

/// Middleware guarding the admin API when a token is configured.
///
/// Deliberately scoped to the method+path pairs the admin API answers, taken
/// from the same [`ReservedRoutes`] the router registered from. Two things
/// stay unguarded on purpose: the health check, which is what liveness probes
/// call and they carry no credentials, and any admin-prefixed path Mimic
/// doesn't itself answer, which is an ordinary mock request.
pub async fn require_admin_token(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(expected) = state.admin_token.as_deref() else {
        return next.run(request).await;
    };
    if !state
        .reserved
        .is_admin_endpoint(request.method().as_str(), request.uri().path())
    {
        return next.run(request).await;
    }

    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .is_some_and(|token| token == expected.as_str());

    if authorized {
        next.run(request).await
    } else {
        warn!(
            "Rejecting {} {}: admin API token missing or incorrect",
            request.method(),
            request.uri().path()
        );
        unauthorized()
    }
}

/// Normalize a configured route path: trimmed, with a leading slash and no
/// trailing one. An empty value means "switched off"; anything unusable falls
/// back to off rather than panicking `Router::route`.
fn normalize_route(raw: &str, var: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.starts_with('/') {
        warn!(
            "Invalid {}='{}': a route must start with '/'. Using '/{}'.",
            var, raw, trimmed
        );
        return Some(format!("/{}", trimmed));
    }
    Some(trimmed.to_string())
}

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
    /// The method+path pairs Mimic answers itself. Shared with the router that
    /// registered them so `/admin/mocks` can mark a shadowed mock unreachable
    /// instead of leaving it at an unexplained `hits: 0`.
    pub reserved: Arc<ReservedRoutes>,
    /// What is scrubbed from the bodies stored on a log entry.
    pub redaction: Arc<BodyRedaction>,
    /// Bearer token the admin API requires, or `None` to leave it open — the
    /// default, and what every Mimic before this did.
    pub admin_token: Option<Arc<String>>,
    /// The CORS settings, or `None` when `MIMIC_CORS` is off — which is the
    /// default, and the state in which nothing in [`crate::cors`] runs.
    ///
    /// Held on the state rather than read from [`cors::configured`] at each use
    /// so a test can exercise a configuration without writing to the process
    /// environment the rest of the suite shares.
    pub cors: Option<Arc<CorsConfig>>,
    /// Proxy/passthrough configuration, or `None` for the original behavior:
    /// an unmatched request gets a 404. Set once at startup from
    /// `MIMIC_PROXY_UPSTREAM`; see [`crate::proxy`].
    pub proxy_config: Option<Arc<crate::proxy::ProxyConfig>>,
    /// Dedupe bookkeeping for `MIMIC_RECORD_UPSTREAM`, shared across every
    /// proxied request so concurrent identical ones don't race to write the
    /// same recording twice.
    pub record_state: Arc<crate::proxy::RecordState>,
    /// Root mocks directory; recordings land under `<mocks_dir>/_recorded/`.
    /// Set from `main`'s resolved mocks directory, defaulted here to
    /// `"mocks"` for tests that never enable proxying and so never read it.
    pub mocks_dir: String,
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
            reserved: Arc::new(ReservedRoutes::default()),
            redaction: Arc::new(BodyRedaction::default()),
            admin_token: None,
            cors: cors::configured().cloned().map(Arc::new),
            proxy_config: None,
            record_state: Arc::new(crate::proxy::RecordState::new()),
            mocks_dir: "mocks".to_string(),
        }
    }

    /// Replace the reserved-route configuration, as `main` does once it has
    /// read the environment. Defaults to `/health` + `/admin`, which is what
    /// every test — and every deployment that configures neither — wants.
    pub fn with_reserved(mut self, reserved: ReservedRoutes) -> Self {
        self.reserved = Arc::new(reserved);
        self
    }

    /// Replace the body-redaction policy. Defaults to
    /// [`DEFAULT_REDACT_BODY_FIELDS`], so a test exercises the same policy a
    /// default deployment runs.
    pub fn with_redaction(mut self, redaction: BodyRedaction) -> Self {
        self.redaction = Arc::new(redaction);
        self
    }

    /// Require a bearer token on the admin API. `None` leaves it open.
    pub fn with_admin_token(mut self, token: Option<String>) -> Self {
        self.admin_token = token.map(Arc::new);
        self
    }

    /// This state with an explicit CORS configuration in place of whatever the
    /// environment said.
    #[cfg(test)]
    pub fn with_cors(mut self, cors: CorsConfig) -> Self {
        self.cors = Some(Arc::new(cors));
        self
    }

    /// Set the proxy/passthrough configuration, as `main` does once it has
    /// read `MIMIC_PROXY_UPSTREAM`. `None` (the default) leaves an unmatched
    /// request answered with the ordinary 404.
    pub fn with_proxy_config(
        mut self,
        proxy_config: Option<Arc<crate::proxy::ProxyConfig>>,
    ) -> Self {
        self.proxy_config = proxy_config;
        self
    }

    /// Set the root mocks directory, so a proxy recording lands under
    /// `<mocks_dir>/_recorded/` instead of the test-only default.
    pub fn with_mocks_dir(mut self, mocks_dir: String) -> Self {
        self.mocks_dir = mocks_dir;
        self
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
    // *other* endpoint is none of this request's business (acquire read lock).
    // When proxying is configured, the body is read unconditionally: a miss
    // here won't be known until after matching, and by then a proxied
    // request needs the body forwarded upstream, not silently dropped.
    let needs_body = {
        let mocks = state.mocks.read().await;
        requires_body(&method_str, &path, &mocks, active_tags.as_ref())
    } || state.proxy_config.is_some();

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
            let resolved = match mock.sequence.as_deref() {
                Some(steps) if !steps.is_empty() => {
                    advance_sequence(&state.sequence_counters, &identity, steps).await
                }
                _ => ResolvedResponse {
                    status: mock.status,
                    response: mock.response.clone(),
                    file: mock.response_file.clone(),
                    bytes: mock.response_bytes.clone(),
                    template: mock.template.unwrap_or(false),
                    delay_ms: None,
                },
            };
            let ResolvedResponse {
                status: status_u16,
                response,
                file: response_file,
                bytes: response_bytes,
                template,
                delay_ms,
            } = resolved;

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
            let file_body = response_bytes.as_ref().map(|bytes| FileBody {
                declared: response_file.as_deref().unwrap_or("(response_file)"),
                bytes,
                template,
            });
            let (header_map, body) = build_response_parts(
                &response,
                file_body.as_ref(),
                &template_ctx,
                mock.response_headers.as_ref(),
                state.cors.as_deref(),
                cors::request_origin(&context),
            );
            let recorded_body = recorded_response_body(
                &body,
                declared_content_type(&header_map),
                file_body.as_ref(),
            );

            // Record the request with the status and response actually served
            let path_params = context.path_params.clone();
            record_request(
                &state,
                context,
                RequestOutcome {
                    matched_mock: Some(matched_key),
                    response_status: status_u16,
                    response_body: Some(recorded_body),
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
            // Gap-fill a CORS preflight before writing this off as a miss: the
            // browser sends `OPTIONS` on its own initiative, and nobody writes
            // a mock for a request they didn't make. An explicit `OPTIONS` mock
            // still wins, because it would have matched above and never reached
            // here.
            if let Some(response) = answer_preflight(&state, &context, active_tags.as_ref()).await {
                return response;
            }

            // Proxy/passthrough: an unmatched request that isn't one of the
            // method+path pairs Mimic answers itself gets one more chance
            // before 404, forwarded live to the configured upstream.
            if let Some(proxy_cfg) = state.proxy_config.clone() {
                if state.reserved.reservation_for(&method_str, &path).is_none() {
                    return handle_proxy_fallback(
                        &state,
                        &proxy_cfg,
                        &method,
                        &uri,
                        &headers,
                        context,
                        &parsed_headers,
                    )
                    .await;
                }
            }

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

/// Answer `context` as a CORS preflight, or `None` to let it fall through to
/// the ordinary 404.
///
/// Only reached once no mock has matched, so this fills a gap rather than
/// hijacking anything: a hand-written `OPTIONS` mock — the way a test
/// reproduces a *broken* preflight — matches first and never gets here.
///
/// A preflight is answered only when the path has a real endpoint behind it for
/// the method the browser is asking about. [`route_exists`] rather than
/// [`find_matching_mock`] decides that, because a preflight carries none of the
/// body or headers the eventual request will, and would be rejected by the very
/// matchers that make the endpoint worth calling.
async fn answer_preflight(
    state: &AppState,
    context: &RequestContext,
    active_tags: Option<&HashSet<String>>,
) -> Option<Response> {
    let config = state.cors.as_deref()?;
    let requested_method = cors::preflight_method(context)?.to_string();

    let has_route = {
        let mocks = state.mocks.read().await;
        route_exists(&requested_method, &context.path, &mocks, active_tags)
    };
    if !has_route {
        // A preflight for a path nobody mocked is still a miss, and the
        // dashboard should say so rather than answering 204 for an endpoint
        // that will 404 a moment later.
        return None;
    }

    info!(
        "CORS preflight answered: OPTIONS {} for {}",
        context.path, requested_method
    );

    let header_map = cors::preflight_headers(config, context);
    // Cloned so the caller keeps its context for the 404 path; preflights are
    // one per endpoint per cache window, not a hot path.
    record_request(
        state,
        context.clone(),
        RequestOutcome {
            matched_mock: None,
            response_status: StatusCode::NO_CONTENT.as_u16(),
            response_body: None,
            response_headers: recorded_response_headers(&header_map),
            match_score: None,
            path_params: HashMap::new(),
            match_explanation: Some(cors::PREFLIGHT_EXPLANATION.to_string()),
        },
    )
    .await;

    Some(assemble_response(
        StatusCode::NO_CONTENT,
        header_map,
        Bytes::new(),
    ))
}

/// Forward an unmatched request to the configured upstream, returning its
/// response to the client and — if `MIMIC_RECORD_UPSTREAM` is enabled —
/// recording it as a new mock in the background.
///
/// `context` is moved in and consumed: this is the terminal handling for the
/// request, mirroring the 404 branch it stands in for.
async fn handle_proxy_fallback(
    state: &AppState,
    proxy_cfg: &crate::proxy::ProxyConfig,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    context: RequestContext,
    parsed_headers: &HashMap<String, String>,
) -> Response {
    let method_str = context.method.clone();
    let path = context.path.clone();
    let body = context.body.clone().unwrap_or_default();

    match crate::proxy::forward(proxy_cfg, method, &path, uri.query(), headers, body).await {
        Ok(upstream) => {
            info!(
                "Proxied {} {} -> {} ({})",
                method_str, path, proxy_cfg.upstream, upstream.status
            );

            if proxy_cfg.record {
                let record_state = state.record_state.clone();
                let mocks_dir = std::path::PathBuf::from(&state.mocks_dir).join("_recorded");
                let record_ctx = context.clone();
                let status = upstream.status.as_u16();
                let resp_headers = upstream.headers.clone();
                let resp_body = upstream.body.clone();
                tokio::spawn(async move {
                    crate::proxy::record_exchange(
                        record_state,
                        mocks_dir,
                        record_ctx,
                        status,
                        resp_headers,
                        resp_body,
                    )
                    .await;
                });
            }

            let response_body_string = String::from_utf8_lossy(&upstream.body).to_string();
            let response_headers_for_log = recorded_response_headers(&upstream.headers);
            let path_params = context.path_params.clone();
            record_request(
                state,
                context,
                RequestOutcome {
                    matched_mock: Some(format!("proxy:{}", proxy_cfg.upstream)),
                    response_status: upstream.status.as_u16(),
                    response_body: Some(response_body_string),
                    response_headers: response_headers_for_log,
                    match_score: None,
                    path_params,
                    match_explanation: Some(format!("proxied to {}", proxy_cfg.upstream)),
                },
            )
            .await;

            let mut res = Response::new(Body::from(upstream.body));
            *res.status_mut() = upstream.status;
            *res.headers_mut() = upstream.headers;
            res
        }
        Err(e) => {
            warn!("Proxy error for {} {}: {}", method_str, path, e);

            let error_body = json!({
                "error": "mock not found",
                "method": method_str,
                "path": path,
                "query_params": context.query_params.clone(),
                "headers_received": parsed_headers.keys().collect::<Vec<_>>(),
                "upstream_error": e.to_string(),
            });

            record_request(
                state,
                context,
                RequestOutcome {
                    matched_mock: None,
                    response_status: 404,
                    response_body: Some(error_body.to_string()),
                    response_headers: HashMap::new(),
                    match_score: None,
                    path_params: HashMap::new(),
                    match_explanation: Some(format!("proxy error: {}", e)),
                },
            )
            .await;

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
                REDACTED.to_string()
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
                (k, REDACTED.to_string())
            } else {
                (k, v)
            }
        })
        .collect();

    let body_limit = max_recorded_body_size();
    let redaction = &state.redaction;
    let request_content_type = context.content_type.clone();
    // A mock returning a token has the same problem the request that carried
    // one does, so the response body goes through the same policy — read
    // against the content type actually being sent.
    let response_content_type = outcome.response_headers.get("content-type").cloned();

    // Redaction runs before truncation, not after: a body cut at 64 KB can
    // stop mid-object, and JSON that no longer parses is JSON whose fields
    // can't be found and scrubbed.
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
            .and_then(|b| redaction.apply(b, request_content_type.as_deref()))
            .map(|b| truncate_body(b, body_limit)),
        matched_mock: outcome.matched_mock,
        response_status: outcome.response_status,
        response_body: outcome
            .response_body
            .and_then(|b| redaction.apply(b, response_content_type.as_deref()))
            .map(|b| truncate_body(b, body_limit)),
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
    reserved: &ReservedRoutes,
) -> serde_json::Value {
    // A mock on a reserved path is loaded, listed, and never served — the one
    // miss the dashboard couldn't previously see, because the request never
    // reaches `handle_request` and so is never recorded.
    let shadowed_by = reserved.reservation_for(&mock.method, &mock.path);

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
        // Whether a request can reach this mock at all, and why not when it
        // can't. `hits: 0` on a reachable mock means "nothing asked for it";
        // on an unreachable one it means "nothing ever can".
        "reachable": shadowed_by.is_none(),
        "unreachable_reason": shadowed_by.map(|owner| {
            format!(
                "{} {} is reserved by {} and will never be served from a mock",
                mock.method, mock.path, owner
            )
        }),
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
                &state.reserved,
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

/// A response body being served from a `response_file` rather than from
/// `response`: the bytes the loader read, plus what's needed to describe them.
struct FileBody<'a> {
    /// The path as written in the mock file. Used to infer a content type and
    /// to name the fixture in the request log.
    declared: &'a str,
    bytes: &'a Bytes,
    /// Whether `{{...}}` inside the file should be rendered.
    template: bool,
}

/// Content types inferred from a `response_file` extension, lowercased.
///
/// Deliberately short: these are the extensions a mock fixture actually turns
/// out to be, and anything else is `application/octet-stream` — a wrong guess
/// on a binary body is worse than no guess. `response_headers` overrides all of
/// it, so an unlisted type is one line away.
const CONTENT_TYPE_BY_EXTENSION: &[(&str, &str)] = &[
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("csv", "text/csv; charset=utf-8"),
    ("html", "text/html; charset=utf-8"),
    ("htm", "text/html; charset=utf-8"),
    ("txt", "text/plain; charset=utf-8"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
];

/// The fallback for a `response_file` whose extension says nothing.
const OCTET_STREAM: &str = "application/octet-stream";

/// The content type a `response_file` is served with when `response_headers`
/// doesn't set one.
fn content_type_for_file(declared: &str) -> &'static str {
    let extension = Path::new(declared)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    extension
        .and_then(|ext| {
            CONTENT_TYPE_BY_EXTENSION
                .iter()
                .find(|(name, _)| *name == ext)
                .map(|(_, value)| *value)
        })
        .unwrap_or(OCTET_STREAM)
}

/// True if a body of this content type is text a human would read — and so is
/// safe to run templating over and worth storing verbatim in the request log.
///
/// Anything else is treated as opaque bytes: never scanned for `{{`, never
/// copied into the log.
pub(crate) fn is_textual_content_type(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/")
        || [
            "json",
            "xml",
            "yaml",
            "csv",
            "html",
            "javascript",
            "graphql",
        ]
        .iter()
        .any(|marker| ct.contains(marker))
        || ct.contains("x-www-form-urlencoded")
}

/// Resolve the headers and serialize the body of a mock response.
///
/// Custom header names are case-insensitive; invalid names/values are skipped
/// with a warning. The content type is the first of: one set by the mock's own
/// `response_headers`; one inferred from a `response_file`'s extension;
/// `application/json`. Two rules follow from it, stated here together because
/// they are the same rule seen from two sides:
///
/// - a `response_file` body is sent as the file's exact bytes, so a PNG stays a
///   PNG and a `.json` fixture is a JSON body rather than a JSON-quoted string;
/// - a `response` body that is a JSON string is sent raw when the content type
///   is not JSON, so XML/CSV/plain-text mocks aren't JSON-quoted either.
///
/// Templating runs over a file body only when the mock opted in with
/// `"template": true` *and* the content type is textual — a binary fixture is
/// never scanned for `{{`.
///
/// Returns the parts rather than a finished [`Response`] so the handler can
/// record exactly what it is about to send, instead of serializing a second
/// copy for the log and trusting the two to agree.
///
/// When `cors` is configured, its headers are added *after* the mock's own, and
/// only for names the mock didn't set — so a mock that returns a deliberately
/// wrong `Access-Control-Allow-Origin` still returns it.
fn build_response_parts(
    response: &serde_json::Value,
    file: Option<&FileBody>,
    template_ctx: &TemplateContext,
    custom_headers: Option<&HashMap<String, String>>,
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
) -> (HeaderMap, Bytes) {
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
    if let Some(config) = cors {
        cors::apply_headers(config, origin, &mut header_map);
    }
    if !header_map.contains_key(CONTENT_TYPE) {
        let inferred = file.map_or("application/json", |f| content_type_for_file(f.declared));
        match axum::http::HeaderValue::from_str(inferred) {
            Ok(value) => {
                header_map.insert(CONTENT_TYPE, value);
            }
            // Unreachable for the table above; a fallback beats an unwrap.
            Err(_) => {
                header_map.insert(
                    CONTENT_TYPE,
                    axum::http::HeaderValue::from_static(OCTET_STREAM),
                );
            }
        }
    }

    let content_type = header_map
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if let Some(file) = file {
        let textual = is_textual_content_type(&content_type);
        let bytes = match (file.template && textual, std::str::from_utf8(file.bytes)) {
            (true, Ok(text)) => Bytes::from(crate::template::render_text(text, template_ctx)),
            // Not templated, not text, or not valid UTF-8: the bytes as read.
            _ => file.bytes.clone(),
        };
        return (header_map, bytes);
    }

    let is_json_content_type = content_type.to_ascii_lowercase().contains("json");
    let body = match response {
        serde_json::Value::String(s) if !is_json_content_type => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };

    (header_map, Bytes::from(body))
}

/// What the request log stores for a body that is about to be sent.
///
/// Text is stored as-is — the dashboard's response drawer is most of the reason
/// the log exists. A binary body is stored as a one-line descriptor instead: a
/// 4 MB PDF rendered as replacement characters helps nobody, and would blow
/// past `MIMIC_MAX_RECORDED_BODY` on every single request that hit the mock.
fn recorded_response_body(body: &Bytes, content_type: &str, file: Option<&FileBody>) -> String {
    if is_textual_content_type(content_type) {
        if let Ok(text) = std::str::from_utf8(body) {
            return text.to_string();
        }
    }

    match file {
        Some(file) => format!(
            "<{} bytes of {} from {}>",
            body.len(),
            content_type,
            file.declared
        ),
        None => format!("<{} bytes of {}>", body.len(), content_type),
    }
}

/// The content type a set of resolved response headers declares.
fn declared_content_type(headers: &HeaderMap) -> &str {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Put an already-resolved status, header set, and body on the wire.
fn assemble_response(status: StatusCode, header_map: HeaderMap, body: Bytes) -> Response {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    *res.headers_mut() = header_map;
    res
}

/// The response a matched mock will serve, once a sequence step (if any) has
/// been picked: everything that differs between "the mock's own response" and
/// "this step's response", in one value so the two paths can't drift.
struct ResolvedResponse {
    status: u16,
    response: serde_json::Value,
    /// `response_file` as written in the mock, for content-type inference.
    file: Option<String>,
    /// Its bytes, read at load time. `None` for an ordinary JSON response.
    bytes: Option<Bytes>,
    template: bool,
    delay_ms: Option<u64>,
}

/// Pick the current sequence step and advance the counter.
/// The write lock is held only for the map lookup + clone, never across an await.
async fn advance_sequence(
    counters: &SequenceCounters,
    identity: &MockIdentity,
    steps: &[SequenceStep],
) -> ResolvedResponse {
    let mut map = counters.write().await;
    let count = map.entry(identity.clone()).or_insert(0);
    // Clamp so the last step keeps repeating once the sequence is exhausted
    let idx = (*count).min(steps.len() - 1);
    let step = &steps[idx];
    if !step.repeat {
        *count += 1;
    }
    ResolvedResponse {
        status: step.status,
        response: step.response.clone(),
        file: step.response_file.clone(),
        bytes: step.response_bytes.clone(),
        template: step.template.unwrap_or(false),
        delay_ms: step.delay_ms,
    }
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                    response_file: None,
                    template: None,
                    response_bytes: None,
                    repeat: true,
                }]),
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                    response_file: None,
                    template: None,
                    response_bytes: None,
                    repeat: true,
                }]),
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                        response_file: None,
                        template: None,
                        response_bytes: None,
                        repeat: false,
                    },
                    SequenceStep {
                        status: 200,
                        response: json!({"ok": true}),
                        delay_ms: None,
                        response_file: None,
                        template: None,
                        response_bytes: None,
                        repeat: true,
                    },
                ]),
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
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
                    response_file: None,
                    template: None,
                    response_bytes: None,
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
                    response_file: None,
                    template: None,
                    response_bytes: None,
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
            response_file: None,
            template: None,
            response_bytes: None,
        }
    }

    fn step(status: u16, body: serde_json::Value) -> SequenceStep {
        SequenceStep {
            status,
            response: body,
            delay_ms: None,
            response_file: None,
            template: None,
            response_bytes: None,
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
            response_file: None,
            template: None,
            response_bytes: None,
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
            response_file: None,
            template: None,
            response_bytes: None,
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
            response_file: None,
            template: None,
            response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"state": "done"}),
                delay_ms: None,
                response_file: None,
                template: None,
                response_bytes: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"state": "done"}),
                delay_ms: None,
                response_file: None,
                template: None,
                response_bytes: None,
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
            response_file: None,
            template: None,
            response_bytes: None,
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
                response_file: None,
                template: None,
                response_bytes: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"n": 2}),
                delay_ms: None,
                response_file: None,
                template: None,
                response_bytes: None,
                repeat: false,
            },
            SequenceStep {
                status: 200,
                response: json!({"n": 3}),
                delay_ms: None,
                response_file: None,
                template: None,
                response_bytes: None,
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

    // ── Reserved routes (#87) ───────────────────────────────────────────

    fn reserved(
        health: Option<&str>,
        admin: Option<&str>,
        disable: Option<&str>,
    ) -> ReservedRoutes {
        ReservedRoutes::from_values(
            health.map(str::to_string),
            admin.map(str::to_string),
            disable.map(str::to_string),
        )
    }

    #[test]
    fn unset_variables_reserve_exactly_what_mimic_has_always_reserved() {
        assert_eq!(reserved(None, None, None), ReservedRoutes::default());
        assert_eq!(
            ReservedRoutes::default().health.as_deref(),
            Some(DEFAULT_HEALTH_PATH)
        );
        assert_eq!(
            ReservedRoutes::default().admin_prefix.as_deref(),
            Some(DEFAULT_ADMIN_PREFIX)
        );
    }

    #[test]
    fn only_the_exact_pairs_mimic_answers_are_reserved() {
        let routes = ReservedRoutes::default();

        assert!(routes.reservation_for("GET", "/health").is_some());
        assert!(routes.reservation_for("GET", "/admin/mocks").is_some());
        assert!(routes.reservation_for("POST", "/admin/scenario").is_some());
        assert!(routes
            .reservation_for("DELETE", "/admin/requests")
            .is_some());

        // These reach the fallback and are perfectly good mocks; warning about
        // them would be noise, and marking them unreachable would be a lie.
        assert_eq!(routes.reservation_for("POST", "/health"), None);
        assert_eq!(routes.reservation_for("POST", "/admin/mocks"), None);
        assert_eq!(routes.reservation_for("GET", "/admin/users"), None);
        assert_eq!(routes.reservation_for("GET", "/healthz"), None);
    }

    #[test]
    fn method_comparison_is_case_insensitive() {
        let routes = ReservedRoutes::default();
        assert!(routes.reservation_for("get", "/health").is_some());
    }

    #[test]
    fn an_empty_value_switches_a_route_off() {
        let routes = reserved(Some(""), None, None);
        assert_eq!(routes.health, None);
        assert_eq!(
            routes.reservation_for("GET", "/health"),
            None,
            "with the health check gone, the path is a mock's to claim"
        );
    }

    #[test]
    fn the_admin_api_can_be_moved_or_switched_off() {
        let moved = reserved(None, Some("/_mimic"), None);
        assert!(moved.reservation_for("GET", "/_mimic/mocks").is_some());
        assert_eq!(moved.reservation_for("GET", "/admin/mocks"), None);

        for truthy in ["true", "1", "yes", "on", " TRUE "] {
            let off = reserved(None, None, Some(truthy));
            assert_eq!(off.admin_prefix, None, "MIMIC_DISABLE_ADMIN={}", truthy);
            assert_eq!(off.reservation_for("GET", "/admin/mocks"), None);
        }

        // A non-affirmative value leaves the admin API where it is, rather
        // than treating "any value at all" as "off".
        assert_eq!(
            reserved(None, None, Some("false")).admin_prefix.as_deref(),
            Some(DEFAULT_ADMIN_PREFIX)
        );
    }

    #[test]
    fn a_prefix_is_normalized_so_it_can_never_panic_the_router() {
        // `Router::route` panics on a path without a leading slash, and a
        // trailing slash would produce "/ops//mocks".
        assert_eq!(
            reserved(None, Some("  /ops/  "), None)
                .admin_prefix
                .as_deref(),
            Some("/ops")
        );
        assert_eq!(
            reserved(None, Some("ops"), None).admin_prefix.as_deref(),
            Some("/ops")
        );
        assert_eq!(reserved(None, Some("/"), None).admin_prefix, None);
    }

    #[test]
    fn disable_wins_over_a_configured_prefix() {
        assert_eq!(
            reserved(None, Some("/ops"), Some("true")).admin_prefix,
            None
        );
    }

    #[tokio::test]
    async fn admin_mocks_marks_a_shadowed_mock_unreachable() {
        // The reported case: a mock for GET /health loads, is listed, and can
        // never serve. `hits: 0` alone doesn't say which of those it is.
        let state = state_with(
            vec![
                mock("GET", "/health", "health-down", &[]),
                mock("GET", "/users", "users", &[]),
            ],
            None,
        );

        let Json(body) = list_mocks(State(state)).await;
        let entries = body["mocks"].as_array().unwrap();

        let health = entries
            .iter()
            .find(|e| e["path"] == "/health")
            .expect("the shadowed mock is still listed");
        assert_eq!(health["reachable"], false);
        assert!(health["unreachable_reason"]
            .as_str()
            .unwrap()
            .contains("reserved by"));

        let users = entries.iter().find(|e| e["path"] == "/users").unwrap();
        assert_eq!(users["reachable"], true);
        assert_eq!(users["unreachable_reason"], serde_json::Value::Null);
    }

    // ── Body redaction (#88) ────────────────────────────────────────────

    fn policy(fields: &[&str]) -> BodyRedaction {
        BodyRedaction {
            fields: fields.iter().map(|f| f.to_string()).collect(),
            disabled: false,
        }
    }

    fn redact(body: &str, content_type: Option<&str>) -> String {
        redact_body(body, content_type, &BodyRedaction::default().fields)
    }

    #[test]
    fn the_default_policy_scrubs_a_password_out_of_a_json_body() {
        // The reported case: the credential sitting next to a header that was
        // already redacted.
        let stored = redact(
            r#"{"username":"alice","password":"hunter2"}"#,
            Some("application/json"),
        );
        let json: serde_json::Value = serde_json::from_str(&stored).unwrap();

        assert_eq!(json["password"], REDACTED);
        assert_eq!(json["username"], "alice", "only the secret is touched");
    }

    #[test]
    fn redaction_reaches_nested_objects_and_arrays() {
        let stored = redact(
            r#"{"users":[{"name":"a","token":"t1"},{"name":"b","token":"t2"}],
                "auth":{"nested":{"api_key":"k"}}}"#,
            Some("application/json"),
        );
        let json: serde_json::Value = serde_json::from_str(&stored).unwrap();

        assert_eq!(json["users"][0]["token"], REDACTED);
        assert_eq!(json["users"][1]["token"], REDACTED);
        assert_eq!(json["auth"]["nested"]["api_key"], REDACTED);
        assert_eq!(json["users"][0]["name"], "a");
    }

    #[test]
    fn a_matching_key_loses_its_whole_subtree() {
        // `{"token": {"value": "..."}}` must not leak because the secret is
        // one level further in than the name that identified it.
        let stored = redact(
            r#"{"token":{"value":"secret","expires":1}}"#,
            Some("application/json"),
        );
        let json: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(json["token"], REDACTED);
    }

    #[test]
    fn field_names_are_matched_case_insensitively_and_exactly() {
        let stored = redact(
            r#"{"Password":"x","PASSWORD":"y","tokenizer":"keep me"}"#,
            Some("application/json"),
        );
        let json: serde_json::Value = serde_json::from_str(&stored).unwrap();

        assert_eq!(json["Password"], REDACTED);
        assert_eq!(json["PASSWORD"], REDACTED);
        assert_eq!(
            json["tokenizer"], "keep me",
            "exact matching: `token` must not swallow `tokenizer`"
        );
    }

    #[test]
    fn form_bodies_are_redacted_field_wise() {
        let stored = redact(
            "username=alice&password=hunter2&remember=1",
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(
            stored,
            format!("username=alice&password={}&remember=1", REDACTED)
        );
    }

    #[test]
    fn a_body_with_nothing_to_redact_is_stored_byte_for_byte() {
        // Reserializing a clean body would rewrite its whitespace, making the
        // log a worse record of what was actually sent.
        let original = "{\n  \"username\": \"alice\"\n}";
        assert_eq!(redact(original, Some("application/json")), original);

        let text = "just some plain text with the word password in it";
        assert_eq!(redact(text, Some("text/plain")), text);
    }

    #[test]
    fn an_empty_field_list_stores_bodies_verbatim() {
        // The documented escape hatch: MIMIC_REDACT_BODY_FIELDS= restores the
        // old behavior for anyone who wants it.
        let verbatim = BodyRedaction::from_values(Some(String::new()), None);
        assert!(verbatim.fields.is_empty());
        assert_eq!(
            verbatim.apply(r#"{"password":"hunter2"}"#.to_string(), None),
            Some(r#"{"password":"hunter2"}"#.to_string())
        );
    }

    #[test]
    fn an_unset_variable_uses_the_default_list_rather_than_nothing() {
        let default = BodyRedaction::from_values(None, None);
        assert_eq!(default, BodyRedaction::default());
        assert!(default.fields.contains("password"));
    }

    #[test]
    fn a_configured_list_replaces_the_default_and_is_trimmed() {
        let configured = BodyRedaction::from_values(Some(" PIN , cvv ,, ".to_string()), None);
        let expected: HashSet<String> = ["pin", "cvv"].iter().map(|f| f.to_string()).collect();
        assert_eq!(configured.fields, expected);
        assert!(
            !configured.fields.contains("password"),
            "an explicit list is the whole list, not an addition to the default"
        );
    }

    #[test]
    fn disabling_the_body_log_stores_nothing() {
        let off = BodyRedaction::from_values(None, Some("true".to_string()));
        assert!(off.disabled);
        assert_eq!(off.apply("anything at all".to_string(), None), None);
    }

    #[tokio::test]
    async fn a_recorded_request_and_response_are_both_redacted() {
        let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
        let mut login = mock("POST", "/login", "login", &[]);
        login.response = json!({"access_token": "very-secret", "user": "alice"});
        login.consume_body = true;
        mocks.insert("POST:/login".to_string(), vec![login]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("authorization", "Bearer super-secret".parse().unwrap());
        handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::from(r#"{"username":"alice","password":"hunter2"}"#),
        )
        .await;

        let log = state.request_log.read().await;
        let record = &log[0];

        let body: serde_json::Value = serde_json::from_str(record.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["password"], REDACTED);
        assert_eq!(body["username"], "alice");

        // A mock returning a token has the same problem the request did.
        let response: serde_json::Value =
            serde_json::from_str(record.response_body.as_ref().unwrap()).unwrap();
        assert_eq!(response["access_token"], REDACTED);
        assert_eq!(response["user"], "alice");

        assert_eq!(record.headers["authorization"], REDACTED);
    }

    #[tokio::test]
    async fn redaction_does_not_change_what_the_client_receives() {
        // Redaction is a property of the log, not of the mock server: the
        // client must still get the token the mock promised.
        let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
        let mut login = mock("POST", "/login", "login", &[]);
        login.response = json!({"access_token": "very-secret"});
        mocks.insert("POST:/login".to_string(), vec![login]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let response = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            HeaderMap::new(),
            State(state),
            Body::empty(),
        )
        .await;

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["access_token"], "very-secret");
    }

    #[tokio::test]
    async fn a_matcher_still_sees_the_real_body() {
        // Matching runs before recording and against the unredacted body, so
        // a mock keyed on a password field keeps working.
        let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
        let mut login = mock("POST", "/login", "matched", &[]);
        login.consume_body = true;
        login.body = Some(crate::types::BodyMatcher::Json(
            crate::types::JsonBodyMatcher {
                exact: None,
                partial: Some(json!({"password": "hunter2"})),
                strict: false,
            },
        ));
        mocks.insert("POST:/login".to_string(), vec![login]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let response = handle_request(
            Method::POST,
            "/login".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::from(r#"{"password":"hunter2"}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        // …and the log still doesn't keep it.
        let log = state.request_log.read().await;
        assert!(!log[0].body.as_ref().unwrap().contains("hunter2"));
    }

    #[tokio::test]
    async fn a_custom_policy_is_honored_end_to_end() {
        let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
        let mut pay = mock("POST", "/pay", "pay", &[]);
        pay.consume_body = true;
        mocks.insert("POST:/pay".to_string(), vec![pay]);
        let state = AppState::new(Arc::new(tokio::sync::RwLock::new(mocks)))
            .with_redaction(policy(&["cvv"]));

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        handle_request(
            Method::POST,
            "/pay".parse().unwrap(),
            headers,
            State(state.clone()),
            Body::from(r#"{"cvv":"123","password":"hunter2"}"#),
        )
        .await;

        let log = state.request_log.read().await;
        let body: serde_json::Value = serde_json::from_str(log[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["cvv"], REDACTED);
        assert_eq!(
            body["password"], "hunter2",
            "a configured list replaces the default entirely"
        );
    }

    // ── Admin API authentication (#88) ──────────────────────────────────

    #[test]
    fn the_token_guards_the_admin_endpoints_and_not_the_health_check() {
        let routes = ReservedRoutes::default();

        assert!(routes.is_admin_endpoint("GET", "/admin/requests"));
        assert!(routes.is_admin_endpoint("DELETE", "/admin/requests"));
        assert!(routes.is_admin_endpoint("GET", "/admin/dashboard"));

        // Liveness probes call this and carry no credentials.
        assert!(!routes.is_admin_endpoint("GET", "/health"));
        // Not an endpoint Mimic answers — an ordinary mock request.
        assert!(!routes.is_admin_endpoint("GET", "/admin/users"));
        assert!(!routes.is_admin_endpoint("PUT", "/admin/requests"));
    }

    #[tokio::test]
    async fn a_mock_is_reachable_once_the_route_it_collided_with_is_freed() {
        let state = state_with(vec![mock("GET", "/health", "health-down", &[])], None)
            .with_reserved(reserved(Some(""), None, None));

        let Json(body) = list_mocks(State(state)).await;
        assert_eq!(body["mocks"][0]["reachable"], true);
    }
}

// ============================================================================
// Built-in CORS (#89)
// ============================================================================

#[cfg(test)]
mod cors_tests {
    use super::*;
    use crate::types::{BodyMatcher, JsonBodyMatcher, MockConfig};
    use axum::http::Method;

    fn mock(method: &str, path: &str) -> MockConfig {
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
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
        }
    }

    fn state_with(mocks: Vec<MockConfig>) -> AppState {
        let mut map: HashMap<String, Vec<MockConfig>> = HashMap::new();
        for m in mocks {
            let key = crate::types::create_mock_key(&m.method, &m.path);
            map.entry(key).or_default().push(m);
        }
        AppState::new(Arc::new(tokio::sync::RwLock::new(map)))
    }

    /// A configuration built from the given `MIMIC_CORS_*` values, without
    /// touching the process environment the rest of the suite shares.
    fn config(pairs: &[(&str, &str)]) -> CorsConfig {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        CorsConfig::from_env(|key| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        })
        .expect("these settings should enable CORS")
    }

    /// The default configuration: `MIMIC_CORS=true` and nothing else.
    fn enabled() -> CorsConfig {
        config(&[("MIMIC_CORS", "true")])
    }

    async fn send(
        state: &AppState,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Response {
        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            header_map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        handle_request(
            method,
            path.parse().unwrap(),
            header_map,
            State(state.clone()),
            Body::empty(),
        )
        .await
    }

    fn header(response: &Response, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    }

    const ORIGIN: (&str, &str) = ("origin", "http://localhost:3000");
    const PREFLIGHT_POST: (&str, &str) = ("access-control-request-method", "POST");

    // ── Off by default ──────────────────────────────────────────────────

    #[tokio::test]
    async fn cors_off_leaves_responses_exactly_as_they_were() {
        let state = state_with(vec![mock("GET", "/users")]);

        let response = send(&state, Method::GET, "/users", &[ORIGIN]).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "access-control-allow-origin"), None);
        assert_eq!(header(&response, "vary"), None);
    }

    #[tokio::test]
    async fn cors_off_still_404s_a_preflight() {
        let state = state_with(vec![mock("POST", "/users")]);

        let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Simple responses ────────────────────────────────────────────────

    #[tokio::test]
    async fn a_simple_response_carries_the_cors_headers() {
        let state = state_with(vec![mock("GET", "/users")]).with_cors(enabled());

        let response = send(&state, Method::GET, "/users", &[ORIGIN]).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, "access-control-allow-origin"),
            Some("*".to_string())
        );
        assert_eq!(
            header(&response, "content-type"),
            Some("application/json".to_string()),
            "the CORS headers must not displace the content type"
        );
    }

    /// Mocking a CORS *failure* has to stay possible, so the mock's own header
    /// is never replaced by the global config.
    #[tokio::test]
    async fn a_mocks_own_header_wins() {
        let mut deliberate = mock("GET", "/users");
        deliberate.response_headers = Some(HashMap::from([(
            "Access-Control-Allow-Origin".to_string(),
            "http://somewhere-else".to_string(),
        )]));
        let state = state_with(vec![deliberate]).with_cors(enabled());

        let response = send(&state, Method::GET, "/users", &[ORIGIN]).await;

        assert_eq!(
            header(&response, "access-control-allow-origin"),
            Some("http://somewhere-else".to_string())
        );
    }

    #[tokio::test]
    async fn an_origin_outside_the_allowlist_gets_no_allow_origin_header() {
        let state = state_with(vec![mock("GET", "/users")]).with_cors(config(&[
            ("MIMIC_CORS", "true"),
            ("MIMIC_CORS_ORIGINS", "http://localhost:3000"),
        ]));

        let allowed = send(&state, Method::GET, "/users", &[ORIGIN]).await;
        assert_eq!(
            header(&allowed, "access-control-allow-origin"),
            Some("http://localhost:3000".to_string())
        );

        let refused = send(
            &state,
            Method::GET,
            "/users",
            &[("origin", "http://evil.example")],
        )
        .await;
        assert_eq!(refused.status(), StatusCode::OK);
        assert_eq!(header(&refused, "access-control-allow-origin"), None);
        assert_eq!(header(&refused, "vary"), Some("Origin".to_string()));
    }

    // ── Preflights ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_preflight_for_a_mocked_path_is_answered_without_a_mock_file() {
        let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

        let response = send(
            &state,
            Method::OPTIONS,
            "/users",
            &[
                ORIGIN,
                PREFLIGHT_POST,
                ("access-control-request-headers", "content-type"),
            ],
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            header(&response, "access-control-allow-origin"),
            Some("*".to_string())
        );
        assert_eq!(
            header(&response, "access-control-allow-methods"),
            Some("GET, POST, PUT, PATCH, DELETE, OPTIONS".to_string())
        );
        assert_eq!(
            header(&response, "access-control-allow-headers"),
            Some("content-type".to_string())
        );
        assert_eq!(
            header(&response, "access-control-max-age"),
            Some("600".to_string())
        );
    }

    #[tokio::test]
    async fn a_preflight_for_an_unmocked_path_still_404s() {
        let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

        let response = send(
            &state,
            Method::OPTIONS,
            "/nowhere",
            &[ORIGIN, PREFLIGHT_POST],
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The preflight asks about POST; only GET is mocked. Answering 204 here
    /// would promise an endpoint that 404s a moment later.
    #[tokio::test]
    async fn a_preflight_for_an_unmocked_method_still_404s() {
        let state = state_with(vec![mock("GET", "/users")]).with_cors(enabled());

        let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A preflight carries no body, so running the endpoint's own matchers
    /// against it would reject every preflight for a mock worth calling.
    #[tokio::test]
    async fn a_preflight_is_answered_for_an_endpoint_behind_a_body_matcher() {
        let mut guarded = mock("POST", "/login");
        guarded.consume_body = true;
        guarded.body = Some(BodyMatcher::Json(JsonBodyMatcher {
            exact: None,
            partial: Some(json!({"user": "admin"})),
            strict: false,
        }));
        let state = state_with(vec![guarded]).with_cors(enabled());

        let response = send(&state, Method::OPTIONS, "/login", &[ORIGIN, PREFLIGHT_POST]).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_preflight_is_answered_for_a_pattern_route() {
        let state = state_with(vec![mock("DELETE", "/users/:id")]).with_cors(enabled());

        let response = send(
            &state,
            Method::OPTIONS,
            "/users/42",
            &[ORIGIN, ("access-control-request-method", "DELETE")],
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    /// Gap-filling, not hijacking: a hand-written `OPTIONS` mock matches first
    /// and never reaches the preflight path.
    #[tokio::test]
    async fn an_explicit_options_mock_wins() {
        let mut explicit = mock("OPTIONS", "/users");
        explicit.status = 418;
        let state = state_with(vec![mock("POST", "/users"), explicit]).with_cors(enabled());

        let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    }

    /// An `OPTIONS` without `Access-Control-Request-Method` isn't a preflight —
    /// it's curl, or a probe — and keeps the behavior it has always had.
    #[tokio::test]
    async fn a_bare_options_is_not_treated_as_a_preflight() {
        let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

        let response = send(&state, Method::OPTIONS, "/users", &[ORIGIN]).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_preflight_from_a_disallowed_origin_gets_no_allow_origin_header() {
        let state = state_with(vec![mock("POST", "/users")]).with_cors(config(&[
            ("MIMIC_CORS", "true"),
            ("MIMIC_CORS_ORIGINS", "http://localhost:3000"),
        ]));

        let response = send(
            &state,
            Method::OPTIONS,
            "/users",
            &[("origin", "http://evil.example"), PREFLIGHT_POST],
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(header(&response, "access-control-allow-origin"), None);
    }

    #[tokio::test]
    async fn a_preflight_honours_a_scenario_filter() {
        let mut tagged = mock("POST", "/checkout");
        tagged.tags = vec!["error-scenario".to_string()];
        let map = HashMap::from([("POST:/checkout".to_string(), vec![tagged])]);
        let state = AppState::with_active_tags(
            Arc::new(tokio::sync::RwLock::new(map)),
            Some(HashSet::from(["happy-path".to_string()])),
        )
        .with_cors(enabled());

        let response = send(
            &state,
            Method::OPTIONS,
            "/checkout",
            &[ORIGIN, PREFLIGHT_POST],
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "a mock the active scenario hides has no endpoint to preflight"
        );
    }

    // ── Observability ───────────────────────────────────────────────────

    #[tokio::test]
    async fn a_preflight_is_recorded_with_an_explanation() {
        let state = state_with(vec![mock("POST", "/users")]).with_cors(enabled());

        send(&state, Method::OPTIONS, "/users", &[ORIGIN, PREFLIGHT_POST]).await;

        let log = state.request_log.read().await;
        assert_eq!(log.len(), 1, "a preflight must not vanish from the log");
        let record = &log[0];
        assert_eq!(record.method, "OPTIONS");
        assert_eq!(record.path, "/users");
        assert_eq!(record.response_status, 204);
        assert_eq!(record.matched_mock, None);
        assert_eq!(
            record.match_explanation.as_deref(),
            Some(crate::cors::PREFLIGHT_EXPLANATION)
        );
        assert_eq!(
            record.response_headers.get("access-control-allow-origin"),
            Some(&"*".to_string())
        );
    }
}

// ============================================================================
// response_file: file-backed response bodies (#90)
// ============================================================================

#[cfg(test)]
mod response_file_tests {
    use super::*;
    use axum::http::Method;
    use std::fs;
    use std::path::Path as FsPath;
    use tempfile::TempDir;

    /// A fixture that is unmistakably not text: every byte value, including a
    /// NUL, a lone `0x80` continuation byte, and a `{{path.id}}` that must
    /// never be treated as a template.
    fn binary_fixture() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(b"{{path.id}}");
        bytes.extend((0u8..=255).rev());
        bytes
    }

    /// FNV-1a, so "byte-identical" is asserted on a digest of the whole body
    /// rather than on its length.
    fn checksum(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ *byte as u64).wrapping_mul(0x100000001b3)
        })
    }

    /// Write `mock` and its fixtures into a fresh mocks directory.
    fn mocks_dir(files: &[(&str, &[u8])]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        dir
    }

    /// Load a mocks directory exactly as the server does and serve from it.
    fn state_from(dir: &FsPath) -> AppState {
        let result = crate::loader::load_mocks_map(dir.to_str().unwrap());
        assert_eq!(result.errors, 0, "fixture set should load cleanly");
        AppState::new(Arc::new(tokio::sync::RwLock::new(result.mocks)))
    }

    async fn get(state: &AppState, path: &str) -> (StatusCode, HeaderMap, Bytes) {
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
        (parts.status, parts.headers, bytes)
    }

    fn content_type(headers: &HeaderMap) -> &str {
        headers.get("content-type").unwrap().to_str().unwrap()
    }

    #[tokio::test]
    async fn serves_a_text_fixture_with_a_content_type_inferred_from_its_extension() {
        let csv = "id,name\n1,Alice\n2,Bob\n";
        let dir = mocks_dir(&[
            (
                "export.json",
                br#"{"method":"GET","path":"/reports/export","status":200,
                     "response_file":"fixtures/report.csv"}"#,
            ),
            ("fixtures/report.csv", csv.as_bytes()),
        ]);

        let (status, headers, body) = get(&state_from(dir.path()), "/reports/export").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type(&headers), "text/csv; charset=utf-8");
        assert_eq!(body, Bytes::from(csv));
    }

    #[tokio::test]
    async fn a_binary_fixture_round_trips_byte_identically() {
        let fixture = binary_fixture();
        let dir = mocks_dir(&[
            (
                "logo.json",
                br#"{"method":"GET","path":"/assets/logo.png","status":200,
                     "response_file":"fixtures/logo.png"}"#,
            ),
            ("fixtures/logo.png", &fixture),
        ]);

        let (status, headers, body) = get(&state_from(dir.path()), "/assets/logo.png").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type(&headers), "image/png");
        assert_eq!(
            checksum(&body),
            checksum(&fixture),
            "the served bytes must digest to the same value as the file"
        );
        assert_eq!(body.len(), fixture.len());
    }

    #[tokio::test]
    async fn a_json_fixture_is_served_as_a_json_body_not_a_quoted_string() {
        let dir = mocks_dir(&[
            (
                "users.json",
                br#"{"method":"GET","path":"/users","status":200,
                     "response_file":"fixtures/users.json"}"#,
            ),
            ("fixtures/users.json", br#"{"users":[{"id":1}]}"#),
        ]);

        let (_, headers, body) = get(&state_from(dir.path()), "/users").await;

        assert_eq!(content_type(&headers), "application/json");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["users"][0]["id"], 1);
    }

    #[tokio::test]
    async fn response_headers_win_over_the_inferred_content_type() {
        let dir = mocks_dir(&[
            (
                "export.json",
                br#"{"method":"GET","path":"/export","status":200,
                     "response_file":"fixtures/report.bin",
                     "response_headers":{"Content-Type":"text/csv"}}"#,
            ),
            ("fixtures/report.bin", b"id,name\n1,Alice\n"),
        ]);

        let (_, headers, body) = get(&state_from(dir.path()), "/export").await;

        assert_eq!(content_type(&headers), "text/csv");
        assert_eq!(body, Bytes::from("id,name\n1,Alice\n"));
    }

    #[tokio::test]
    async fn an_unknown_extension_falls_back_to_octet_stream() {
        let dir = mocks_dir(&[
            (
                "blob.json",
                br#"{"method":"GET","path":"/blob","status":200,
                     "response_file":"fixtures/thing.wat"}"#,
            ),
            ("fixtures/thing.wat", b"\x00\x01\x02"),
        ]);

        let (_, headers, _) = get(&state_from(dir.path()), "/blob").await;
        assert_eq!(content_type(&headers), "application/octet-stream");
    }

    #[tokio::test]
    async fn templating_is_off_by_default_for_a_file_body() {
        let dir = mocks_dir(&[
            (
                "user.json",
                br#"{"method":"GET","path":"/users/:id/card","status":200,
                     "response_file":"fixtures/card.html"}"#,
            ),
            ("fixtures/card.html", b"<p>user {{path.id}}</p>"),
        ]);

        let (_, _, body) = get(&state_from(dir.path()), "/users/42/card").await;
        assert_eq!(
            body,
            Bytes::from("<p>user {{path.id}}</p>"),
            "a fixture must not be interpolated unless the mock asks for it"
        );
    }

    #[tokio::test]
    async fn template_true_interpolates_a_text_fixture() {
        let dir = mocks_dir(&[
            (
                "user.json",
                br#"{"method":"GET","path":"/users/:id/card","status":200,
                     "template":true,
                     "response_file":"fixtures/card.html"}"#,
            ),
            (
                "fixtures/card.html",
                b"<p>user {{path.id}} sorted by {{query.sort}}</p>",
            ),
        ]);

        let (_, _, body) = get(&state_from(dir.path()), "/users/42/card?sort=name").await;
        assert_eq!(body, Bytes::from("<p>user 42 sorted by name</p>"));
    }

    #[tokio::test]
    async fn templating_never_runs_on_a_binary_body() {
        // The fixture contains a literal `{{path.id}}`; a PNG is bytes, and
        // opting in must not change that.
        let fixture = binary_fixture();
        let dir = mocks_dir(&[
            (
                "logo.json",
                br#"{"method":"GET","path":"/assets/:id/logo.png","status":200,
                     "template":true,
                     "response_file":"fixtures/logo.png"}"#,
            ),
            ("fixtures/logo.png", &fixture),
        ]);

        let (_, headers, body) = get(&state_from(dir.path()), "/assets/42/logo.png").await;

        assert_eq!(content_type(&headers), "image/png");
        assert_eq!(checksum(&body), checksum(&fixture));
    }

    #[tokio::test]
    async fn a_templated_file_body_can_read_the_request_body() {
        // `{{body.…}}` lives in the fixture, not in the mock JSON, so the
        // decision to read the request body has to look inside the file.
        let dir = mocks_dir(&[
            (
                "echo.json",
                br#"{"method":"POST","path":"/echo","status":200,
                     "template":true,
                     "response_file":"fixtures/echo.txt"}"#,
            ),
            ("fixtures/echo.txt", b"you said: {{body.message}}"),
        ]);

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let response = handle_request(
            Method::POST,
            "/echo".parse().unwrap(),
            headers,
            State(state_from(dir.path())),
            Body::from(r#"{"message":"hi"}"#),
        )
        .await;
        let body = response.into_body().collect().await.unwrap().to_bytes();

        assert_eq!(body, Bytes::from("you said: hi"));
    }

    #[tokio::test]
    async fn a_sequence_step_can_serve_a_file() {
        let dir = mocks_dir(&[
            (
                "flaky.json",
                br#"{"method":"GET","path":"/flaky","status":200,
                     "response":{"ok":true},
                     "sequence":[
                       {"status":503,"response":{"error":"unavailable"}},
                       {"status":200,"response_file":"fixtures/ok.csv","repeat":true}
                     ]}"#,
            ),
            ("fixtures/ok.csv", b"id\n1\n"),
        ]);
        let state = state_from(dir.path());

        let (status, _, _) = get(&state, "/flaky").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, headers, body) = get(&state, "/flaky").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type(&headers), "text/csv; charset=utf-8");
        assert_eq!(body, Bytes::from("id\n1\n"));
    }

    #[tokio::test]
    async fn the_request_log_records_a_descriptor_for_a_binary_body() {
        let fixture = binary_fixture();
        let dir = mocks_dir(&[
            (
                "logo.json",
                br#"{"method":"GET","path":"/logo.png","status":200,
                     "response_file":"fixtures/logo.png"}"#,
            ),
            ("fixtures/logo.png", &fixture),
        ]);
        let state = state_from(dir.path());

        let (_, _, body) = get(&state, "/logo.png").await;

        let log = state.request_log.read().await;
        let recorded = log[0].response_body.as_deref().unwrap();
        assert!(
            recorded.contains(&format!("{} bytes", body.len()))
                && recorded.contains("image/png")
                && recorded.contains("fixtures/logo.png"),
            "unhelpful descriptor: {}",
            recorded
        );
        assert!(
            recorded.len() < 200,
            "the log must not carry the bytes: {} chars",
            recorded.len()
        );
    }

    #[tokio::test]
    async fn the_request_log_keeps_a_text_body_verbatim() {
        let dir = mocks_dir(&[
            (
                "export.json",
                br#"{"method":"GET","path":"/export","status":200,
                     "response_file":"fixtures/report.csv"}"#,
            ),
            ("fixtures/report.csv", b"id,name\n1,Alice\n"),
        ]);
        let state = state_from(dir.path());

        let _ = get(&state, "/export").await;

        let log = state.request_log.read().await;
        assert_eq!(
            log[0].response_body.as_deref(),
            Some("id,name\n1,Alice\n"),
            "a CSV body is exactly what the response drawer is for"
        );
    }

    #[tokio::test]
    async fn a_reload_picks_up_a_changed_fixture() {
        // The mock file is untouched; only the fixture changes. Bytes are read
        // at load time, so this is the case that proves they're re-read.
        let dir = mocks_dir(&[
            (
                "export.json",
                br#"{"method":"GET","path":"/export","status":200,
                     "response_file":"fixtures/report.csv"}"#,
            ),
            ("fixtures/report.csv", b"id\n1\n"),
        ]);
        let state = state_from(dir.path());

        let (_, _, body) = get(&state, "/export").await;
        assert_eq!(body, Bytes::from("id\n1\n"));

        fs::write(dir.path().join("fixtures/report.csv"), b"id\n1\n2\n").unwrap();
        let reloaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(reloaded.errors, 0);
        *state.mocks.write().await = reloaded.mocks;

        let (_, _, body) = get(&state, "/export").await;
        assert_eq!(body, Bytes::from("id\n1\n2\n"));
    }

    // ------------------------------------------------------------------
    // Content-type inference, in isolation
    // ------------------------------------------------------------------

    #[test]
    fn content_type_inference_covers_the_documented_extensions() {
        for (file, expected) in [
            ("a/b/report.json", "application/json"),
            ("feed.xml", "application/xml"),
            ("report.csv", "text/csv; charset=utf-8"),
            ("page.html", "text/html; charset=utf-8"),
            ("notes.txt", "text/plain; charset=utf-8"),
            ("logo.PNG", "image/png"),
            ("photo.jpg", "image/jpeg"),
            ("photo.jpeg", "image/jpeg"),
            ("invoice.pdf", "application/pdf"),
            ("export.zip", "application/zip"),
            ("mystery.bin", "application/octet-stream"),
            ("no_extension", "application/octet-stream"),
        ] {
            assert_eq!(content_type_for_file(file), expected, "for {}", file);
        }
    }

    #[test]
    fn only_textual_content_types_are_templated_and_logged() {
        for textual in [
            "text/plain; charset=utf-8",
            "text/csv",
            "application/json",
            "application/xml",
            "application/soap+xml",
            "text/html",
        ] {
            assert!(is_textual_content_type(textual), "{} is text", textual);
        }
        for binary in [
            "image/png",
            "application/pdf",
            "application/zip",
            "application/octet-stream",
        ] {
            assert!(!is_textual_content_type(binary), "{} is not text", binary);
        }
    }

    // ========================================================================
    // Proxy / record-and-replay (#60)
    // ========================================================================

    mod proxy_tests {
        use super::*;
        use crate::proxy::ProxyConfig;
        use axum::routing::get;
        use axum::Router;
        use tokio::net::TcpListener;

        /// A tiny upstream that counts hits and answers `GET /widgets/1`
        /// with a fixed JSON body. Returns its base URL and the shared hit
        /// counter.
        async fn spawn_counting_upstream() -> (String, Arc<AtomicU64>) {
            let hits = Arc::new(AtomicU64::new(0));
            let counter = hits.clone();

            let app = Router::new().route(
                "/widgets/1",
                get(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"id": 1, "name": "Widget"}))
                    }
                }),
            );

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            (format!("http://{}", addr), hits)
        }

        /// An upstream that accepts the connection and then never responds,
        /// so a client-side timeout is what ends the request.
        async fn spawn_hanging_upstream() -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    if let Ok((socket, _)) = listener.accept().await {
                        // Hold the connection open without ever writing a
                        // response; the client's own timeout must fire.
                        std::mem::forget(socket);
                    }
                }
            });
            format!("http://{}", addr)
        }

        fn proxy_state(mocks_dir: &std::path::Path, upstream: &str, record: bool) -> AppState {
            let mut state = create_empty_state();
            state.mocks_dir = mocks_dir.to_str().unwrap().to_string();
            state.proxy_config = Some(Arc::new(ProxyConfig::for_test(
                upstream.to_string(),
                record,
                2000,
            )));
            state
        }

        #[tokio::test]
        async fn unmatched_request_is_forwarded_to_the_upstream_and_the_response_passed_through() {
            let (upstream, hits) = spawn_counting_upstream().await;
            let dir = tempfile::tempdir().unwrap();
            let state = proxy_state(dir.path(), &upstream, false);

            let response = handle_request(
                Method::GET,
                "/widgets/1".parse().unwrap(),
                HeaderMap::new(),
                State(state),
                Body::empty(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::OK);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["name"], "Widget");
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn upstream_timeout_falls_back_to_a_404_with_an_upstream_error_field() {
            let upstream = spawn_hanging_upstream().await;
            let dir = tempfile::tempdir().unwrap();
            let mut state = create_empty_state();
            state.mocks_dir = dir.path().to_str().unwrap().to_string();
            state.proxy_config = Some(Arc::new(ProxyConfig::for_test(upstream, false, 100)));

            let response = handle_request(
                Method::GET,
                "/widgets/1".parse().unwrap(),
                HeaderMap::new(),
                State(state),
                Body::empty(),
            )
            .await;

            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"], "mock not found");
            assert!(
                json["upstream_error"].is_string(),
                "a timed-out proxy attempt must surface why: {}",
                json
            );
        }

        #[tokio::test]
        async fn reserved_health_and_admin_routes_are_never_proxied() {
            // Proxying is configured, and neither path exists as a mock —
            // if the reservation check were missing, these would reach the
            // (nonexistent) upstream route instead of the ordinary 404 shape.
            // Only the exact method+path pairs `ReservedRoutes` protects are
            // covered here; an admin-adjacent path Mimic doesn't itself
            // answer (e.g. `/admin/not-a-real-route`) is deliberately *not*
            // reserved — see `ReservedRoutes::reservation_for` — and is free
            // to be proxied like any other unmocked request.
            let (upstream, hits) = spawn_counting_upstream().await;
            let dir = tempfile::tempdir().unwrap();
            let state = proxy_state(dir.path(), &upstream, false);

            for path in ["/health", "/admin/dashboard"] {
                let response = handle_request(
                    Method::GET,
                    path.parse().unwrap(),
                    HeaderMap::new(),
                    State(state.clone()),
                    Body::empty(),
                )
                .await;

                assert_eq!(response.status(), StatusCode::NOT_FOUND);
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(json["error"], "mock not found");
                assert!(
                    json.get("upstream_error").is_none(),
                    "{} must never reach the proxy",
                    path
                );
            }
            assert_eq!(hits.load(Ordering::SeqCst), 0);
        }

        /// The full record-and-replay loop from the issue's acceptance
        /// criteria: a proxied response is written to `mocks/_recorded/`,
        /// and once that file is loaded back in (simulating the hot-reload
        /// main.rs runs every few seconds), the *second* identical request
        /// is answered locally — the upstream must see exactly one hit.
        #[tokio::test]
        async fn recorded_response_is_replayed_on_the_next_identical_request_without_a_second_upstream_hit(
        ) {
            let (upstream, hits) = spawn_counting_upstream().await;
            let dir = tempfile::tempdir().unwrap();
            let state = proxy_state(dir.path(), &upstream, true);

            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                "Bearer super-secret-token".parse().unwrap(),
            );

            let first = handle_request(
                Method::GET,
                "/widgets/1".parse().unwrap(),
                headers.clone(),
                State(state.clone()),
                Body::empty(),
            )
            .await;
            assert_eq!(first.status(), StatusCode::OK);
            assert_eq!(hits.load(Ordering::SeqCst), 1);

            // The write happens on a detached task; poll briefly for the
            // recorded file to land instead of guessing a fixed sleep.
            let recorded_dir = dir.path().join("_recorded");
            let mut recorded_file = None;
            for _ in 0..50 {
                if recorded_dir.is_dir() {
                    let mut entries: Vec<_> = std::fs::read_dir(&recorded_dir)
                        .unwrap()
                        .filter_map(|e| e.ok())
                        .collect();
                    if !entries.is_empty() {
                        recorded_file = Some(entries.remove(0).path());
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let recorded_file = recorded_file.expect("a mock should have been recorded");

            let contents = std::fs::read_to_string(&recorded_file).unwrap();
            assert!(
                !contents.contains("super-secret-token"),
                "the Authorization header value must never reach disk: {}",
                contents
            );

            // Simulate the hot-reload main.rs runs on an interval: load what
            // landed on disk into a fresh mock store.
            let loaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
            assert_eq!(loaded.errors, 0);
            {
                let mut mocks = state.mocks.write().await;
                *mocks = loaded.mocks;
            }

            let second = handle_request(
                Method::GET,
                "/widgets/1".parse().unwrap(),
                headers,
                State(state),
                Body::empty(),
            )
            .await;

            assert_eq!(second.status(), StatusCode::OK);
            let bytes = second.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["name"], "Widget");
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "the second identical request must be served from the recorded mock, not the upstream again"
            );
        }
    }
}
