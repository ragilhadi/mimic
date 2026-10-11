use crate::cors::{self, CorsConfig};
use crate::matcher::{
    explain_match, explain_no_match, find_matching_mock, is_pattern_path, parse_body,
    parse_headers, parse_query_string, parse_query_string_multi, requires_body, route_exists,
    MatchResult, RequestContext,
};
use crate::template::{render_response_ref, TemplateContext};
use crate::types::{
    is_sensitive_header, is_truthy, parse_active_tags, ActiveTags, MockConfig, MockIdentity,
    MockStore, RequestLog, RequestRecord, SequenceCounters, SequenceStep,
};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header::CONTENT_LENGTH, header::CONTENT_TYPE, HeaderMap, Method, StatusCode, Uri},
    response::{Html, IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
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
    /// This is the set mocks are checked against — one array, so a route can't
    /// be reserved in the diagnostic and unregistered in the router, or the
    /// reverse. The health check is deliberately outside it: liveness probes
    /// call it and carry no credentials.
    pub fn is_admin_endpoint(&self, method: &str, path: &str) -> bool {
        self.matched_admin_route(method, path).is_some()
    }

    /// Whether `method path` is an admin endpoint the bearer token guards.
    ///
    /// Everything but the dashboard's own `GET`: the shell has to reach the
    /// browser before it can ask for anything, and it is a compiled-in
    /// constant with no request-specific content — nothing to protect beyond
    /// "a Mimic is running here", which the unguarded health check already
    /// says. Every data endpoint stays guarded (#113): the page presents a
    /// token for them, the API keeps its gate.
    pub fn is_token_guarded(&self, method: &str, path: &str) -> bool {
        match self.matched_admin_route(method, path) {
            Some((route, _)) => !(route == "/dashboard" && method.eq_ignore_ascii_case("GET")),
            None => false,
        }
    }

    /// The `ADMIN_ROUTES` entry `method path` would hit, or `None`. The one
    /// place that translates a requested route into the admin API's own —
    /// [`ReservedRoutes::is_admin_endpoint`] and the token guard decide what
    /// they each do with the answer.
    fn matched_admin_route(
        &self,
        method: &str,
        path: &str,
    ) -> Option<(&'static str, &'static [&'static str])> {
        let prefix = self.admin_prefix.as_deref()?;
        let suffix = path.strip_prefix(prefix)?;
        ADMIN_ROUTES.iter().find_map(|(route, methods)| {
            (*route == suffix
                && methods
                    .iter()
                    .any(|allowed| method.eq_ignore_ascii_case(allowed)))
            .then_some((*route, *methods))
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
fn unauthorized(cors: Option<&CorsConfig>, origin: Option<&str>) -> Response {
    cors_wrap(
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
            .into_response(),
        cors,
        origin,
    )
}

/// Add CORS headers — when configured — to a response Mimic builds itself.
///
/// The matched-mock path gets its CORS headers from [`build_response_parts`];
/// this is the same [`cors::apply_headers`] call for the four responses Mimic
/// hand-builds (404 "mock not found", 413, 401, and the proxy fallback) so
/// none of them can quietly skip CORS the way they used to.
fn cors_wrap(mut response: Response, cors: Option<&CorsConfig>, origin: Option<&str>) -> Response {
    if let Some(config) = cors {
        cors::apply_headers(config, origin, response.headers_mut());
    }
    response
}

/// Middleware guarding the admin API when a token is configured.
///
/// Deliberately scoped to the method+path pairs the admin API answers, taken
/// from the same [`ReservedRoutes`] the router registered from. Three things
/// stay unguarded on purpose: the health check, which is what liveness probes
/// call and they carry no credentials, the dashboard's own page load — the
/// shell must reach the browser before it can present a token (#113), and it
/// is a constant with no data in it — and any admin-prefixed path Mimic
/// doesn't itself answer, which is an ordinary mock request. Everything else,
/// the data endpoints the dashboard feeds from, still answers 401 without the
/// token.
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
        .is_token_guarded(request.method().as_str(), request.uri().path())
    {
        return next.run(request).await;
    }

    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token)
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));

    if authorized {
        next.run(request).await
    } else {
        warn!(
            "Rejecting {} {}: admin API token missing or incorrect",
            request.method(),
            request.uri().path()
        );
        let origin = request
            .headers()
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        unauthorized(state.cors.as_deref(), origin)
    }
}

/// The credentials of an `Authorization: Bearer <token>` value.
///
/// The scheme is case-insensitive (RFC 9110 §11.1), so `bearer` and `BEARER`
/// are accepted; the token itself is compared exactly.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// Equality that doesn't stop at the first differing byte, so how long a
/// rejected token took says nothing about how much of it was right. A length
/// mismatch is folded into the result rather than returned early.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
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
            request_log: Arc::new(tokio::sync::RwLock::new(std::collections::VecDeque::new())),
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
fn push_bounded(
    log: &mut std::collections::VecDeque<RequestRecord>,
    record: RequestRecord,
    cap: usize,
) {
    log.push_back(record);
    if cap > 0 && log.len() > cap {
        let excess = log.len() - cap;
        log.drain(..excess);
    }
}

/// The port the server is configured to listen on.
///
/// Lives here so `/health` reports the same number `main` binds to rather than
/// re-deriving it and risking disagreement. An unusable value reads as the
/// default; startup goes through [`configured_port_checked`] first, so by the
/// time anything serves a request the value is known to be valid.
pub fn configured_port() -> u16 {
    configured_port_checked().unwrap_or(DEFAULT_PORT)
}

/// Port used when `PORT` is unset or empty.
const DEFAULT_PORT: u16 = 8080;

/// [`configured_port`] that reports a `PORT` that is set but unusable instead
/// of quietly serving on another port — the same policy as
/// [`configured_bind_address`]: a listener that isn't where the operator said
/// it would be is worse than one that refuses to start.
pub fn configured_port_checked() -> Result<u16, String> {
    parse_port(std::env::var("PORT").ok())
}

fn parse_port(value: Option<String>) -> Result<u16, String> {
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(DEFAULT_PORT),
        Some(raw) => raw.parse::<u16>().map_err(|_| {
            format!(
                "PORT is set to '{}', which is not a valid port number (0-65535)",
                raw
            )
        }),
    }
}

/// Environment variable overriding the address Mimic binds to.
pub const BIND_ADDRESS_ENV: &str = "MIMIC_BIND_ADDRESS";

/// The address the server is configured to listen on, read from
/// `MIMIC_BIND_ADDRESS`. Unset defaults to `0.0.0.0` — every interface — which
/// is what every Mimic before this setting existed always did.
///
/// Unlike `configured_port`, a value that's set but doesn't parse is not
/// silently discarded in favor of the default: binding a wider address than
/// the operator asked for is exactly the outcome this setting exists to
/// prevent, so an unparsable value is a startup error instead.
pub fn configured_bind_address() -> Result<IpAddr, String> {
    parse_bind_address(std::env::var(BIND_ADDRESS_ENV).ok())
}

fn parse_bind_address(value: Option<String>) -> Result<IpAddr, String> {
    match value {
        None => Ok(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        Some(raw) => raw.trim().parse::<IpAddr>().map_err(|_| {
            format!(
                "{} is set to '{}', which is not a valid IPv4 or IPv6 address \
                 (examples: 127.0.0.1, 0.0.0.0, ::1, ::)",
                BIND_ADDRESS_ENV, raw
            )
        }),
    }
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
    // The request path as the client sent it, for forwarding upstream untouched;
    // everything else — matching, `{{path.x}}`, the request log — sees it
    // percent-decoded per segment, the way query strings and forms already are.
    let raw_path = uri.path().to_string();
    let path = crate::matcher::decode_path(&raw_path);
    let method_str = method.as_str().to_string();

    debug!("Incoming request: {} {}", method_str, uri);

    // Parse query parameters from URI
    let query_params = parse_query_string(uri.query());
    let query_params_all = parse_query_string_multi(uri.query());

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
                return payload_too_large(
                    &method_str,
                    &path,
                    max_body,
                    state.cors.as_deref(),
                    parsed_headers.get("origin").map(String::as_str),
                );
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
        query_params_all,
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
            // Whether the mock only won because a `HEAD` request was answered
            // from the mock `GET` would have matched (#112) — read before the
            // destructure, like the score and explanation above.
            let head_fallback = result.head_from_get;
            let MatchResult {
                mut mock,
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
                    // Moved, not cloned: nothing below reads `mock.response`
                    // again, and it can be the bulk of the mock.
                    response: std::mem::take(&mut mock.response),
                    file: mock.response_file.clone(),
                    bytes: mock.response_bytes.clone(),
                    template: mock.template.unwrap_or(false),
                    template_escape: mock.template_escape.unwrap_or(true),
                    delay_ms: None,
                },
            };
            let ResolvedResponse {
                status: status_u16,
                response,
                file: response_file,
                bytes: response_bytes,
                template,
                template_escape,
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
            let response = render_response_ref(&response, &template_ctx);

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
                escape: template_escape,
            });
            let (mut header_map, built_body) = build_response_parts(
                &response,
                file_body.as_ref(),
                &template_ctx,
                mock.response_headers.as_ref(),
                state.cors.as_deref(),
                cors::request_origin(&context),
            );

            // A `HEAD` answered from the GET mock (#112) carries the GET's
            // status and headers with no body — RFC 9110 defines HEAD as GET
            // without one — except `Content-Length`, which reports the size of
            // the body the GET would have sent so a client sizing a download
            // gets the right answer. A `Content-Length` the mock declared
            // itself is left alone, the same precedence the content type gets.
            // What the client never receives, the log stores only as a
            // descriptor, in the shape a binary body is already recorded in.
            let (response_body, recorded_body) = if head_fallback {
                let declared_ct = declared_content_type(&header_map).to_string();
                if !header_map.contains_key(CONTENT_LENGTH) {
                    match axum::http::HeaderValue::from_str(&built_body.len().to_string()) {
                        Ok(value) => {
                            header_map.insert(CONTENT_LENGTH, value);
                        }
                        Err(_) => warn!("Skipping invalid Content-Length: {}", built_body.len()),
                    }
                }
                (
                    Bytes::new(),
                    format!(
                        "<{} bytes of {} — body withheld for HEAD>",
                        built_body.len(),
                        declared_ct
                    ),
                )
            } else {
                let recorded_body = recorded_response_body(
                    &built_body,
                    declared_content_type(&header_map),
                    file_body.as_ref(),
                );
                (built_body, recorded_body)
            };

            // Record the request with the status and response actually served
            let path_params = context.path_params.clone();
            record_request(
                &state,
                context,
                RequestOutcome {
                    matched_mock: Some(matched_key),
                    response_status: status.as_u16(),
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
            assemble_response(status, header_map, response_body)
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
            cors_wrap(
                (StatusCode::NOT_FOUND, Json(error_body)).into_response(),
                state.cors.as_deref(),
                parsed_headers.get("origin").map(String::as_str),
            )
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

    match crate::proxy::forward(proxy_cfg, method, uri.path(), uri.query(), headers, body).await {
        Ok(upstream) => {
            info!(
                "Proxied {} {} -> {} ({})",
                method_str, path, proxy_cfg.upstream, upstream.status
            );

            // Only a response read in full can be recorded; one that was
            // streamed past the buffer limit is forwarded but not written.
            if proxy_cfg.record {
                match upstream.body.buffered() {
                    Some(resp_body) => {
                        let record_state = state.record_state.clone();
                        let mocks_dir =
                            std::path::PathBuf::from(&state.mocks_dir).join("_recorded");
                        let record_ctx = context.clone();
                        let status = upstream.status.as_u16();
                        let resp_headers = upstream.headers.clone();
                        let resp_body = resp_body.clone();
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
                    None => info!(
                        "Not recording {} {}: the response is larger than the {}-byte proxy buffer",
                        method_str, path, proxy_cfg.buffer_limit
                    ),
                }
            }

            // Text is logged as text; a binary or streamed body as a
            // descriptor, the way mock responses are — not as replacement
            // characters, and not as a second full copy of a large download.
            let upstream_content_type = declared_content_type(&upstream.headers).to_string();
            let response_body_string = match upstream.body.buffered() {
                Some(bytes) => recorded_response_body(bytes, &upstream_content_type, None),
                None => format!(
                    "<streamed response of {}, not logged>",
                    if upstream_content_type.is_empty() {
                        "unknown content type"
                    } else {
                        &upstream_content_type
                    }
                ),
            };
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

            let mut res = Response::new(upstream.body.into_body());
            *res.status_mut() = upstream.status;
            *res.headers_mut() = upstream.headers;
            cors_wrap(
                res,
                state.cors.as_deref(),
                parsed_headers.get("origin").map(String::as_str),
            )
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

            cors_wrap(
                (StatusCode::NOT_FOUND, Json(error_body)).into_response(),
                state.cors.as_deref(),
                parsed_headers.get("origin").map(String::as_str),
            )
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
fn payload_too_large(
    method: &str,
    path: &str,
    max_body: usize,
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
) -> Response {
    cors_wrap(
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": "payload too large",
                "method": method,
                "path": path,
                "max_body_size": max_body
            })),
        )
            .into_response(),
        cors,
        origin,
    )
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
        "bind_address": configured_bind_address()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| "0.0.0.0".to_string()),
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
        query_params: context.query_params_all,
        headers: redacted_headers,
        body: context
            .body
            .filter(|b| !b.is_empty())
            .map(|b| match std::str::from_utf8(&b) {
                Ok(text) => text.to_string(),
                // A body that isn't text is recorded as a descriptor rather
                // than left out — an empty log entry reads as "no body sent".
                Err(_) => format!(
                    "<{} bytes of {}>",
                    b.len(),
                    request_content_type
                        .as_deref()
                        .unwrap_or("application/octet-stream")
                ),
            })
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
    /// How many matching records to return, applied *after* every filter
    /// above. Unset uses [`DEFAULT_REQUEST_PAGE_LIMIT`]; `0` returns every
    /// match — the pre-#111 behavior, kept for anyone scripting against this
    /// endpoint who wants it all in one response.
    pub limit: Option<String>,
    /// How many of the most recent matches to skip before taking `limit` —
    /// `offset=0` (the default) is the most recent page, `offset=50` the
    /// page before that, and so on back through the log.
    pub offset: Option<String>,
}

/// Records returned in one page when [`RequestFilter::limit`] is unset.
///
/// Small enough that even a slow connection renders it well under a second;
/// large enough that a normal debugging session rarely needs a second page.
const DEFAULT_REQUEST_PAGE_LIMIT: usize = 50;

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
        || record.query_params.iter().any(|(k, values)| {
            k.to_lowercase().contains(needle)
                || values.iter().any(|v| v.to_lowercase().contains(needle))
        })
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

    let total = filtered.len();

    let limit = filter
        .limit
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_REQUEST_PAGE_LIMIT);
    let offset = filter
        .offset
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    // `offset` counts back from the most recent match, so the default page
    // (`offset=0`) is "the last `limit` requests" — the shape #111 asked for
    // — and a larger offset pages further into the past. `limit=0` is the
    // explicit opt-out: every match, exactly as this endpoint always
    // returned before pagination existed.
    let page: &[&RequestRecord] = if limit == 0 {
        &filtered
    } else {
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        &filtered[start..end]
    };

    Json(json!({
        // Total matches, unchanged from what `count` has always meant — an
        // existing consumer that reads `count` for "how many requests
        // matched" keeps seeing the right number even though `requests` is
        // now a page of them.
        "count": total,
        "returned": page.len(),
        "limit": limit,
        "offset": offset,
        "requests": page
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
        // A mock is loaded and listed here even when its declared `status`
        // can't be put on the wire — the request log would otherwise be the
        // only place that disagreement showed up, and only after a request.
        "servable": crate::types::is_valid_status(mock.status),
        "unservable_reason": (!crate::types::is_valid_status(mock.status)).then(|| {
            format!(
                "status {} is outside 100-599 and will be served as 200 OK",
                mock.status
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
    /// Whether rendered values are escaped for the content type.
    escape: bool,
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
pub(crate) fn content_type_for_file(declared: &str) -> &'static str {
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
            (true, Ok(text)) => {
                let escape = if file.escape {
                    crate::template::Escape::for_content_type(&content_type)
                } else {
                    crate::template::Escape::None
                };
                Bytes::from(crate::template::render_text_escaped(
                    text,
                    template_ctx,
                    escape,
                ))
            }
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
    template_escape: bool,
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
        template_escape: step.template_escape.unwrap_or(true),
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
mod cors_tests;
#[cfg(test)]
mod parsing_tests;
#[cfg(test)]
mod proxy_passthrough_tests;
#[cfg(test)]
mod response_file_tests;
#[cfg(test)]
mod scenario_tests;
#[cfg(test)]
mod tests;
