use crate::auth::{Credential, Denied};
use crate::state::AppState;
use axum::{
    extract::{
        rejection::QueryRejection, DefaultBodyLimit, FromRequestParts, Query as AxumQuery, Request,
        State,
    },
    http::{header, request::Parts, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::{get, post},
    Router,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tailflow_core::{
    processor::Filter,
    query::{self, Query, DEFAULT_CONTEXT_LINES, DEFAULT_MAX_PAYLOAD_CHARS},
    LogLevel,
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

/// The compiled web UI, embedded at build time from `../../web/dist`.
/// Run `npm run build` in the `web/` directory before `cargo build`.
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // API routes — matched before the static fallback
        .route("/events", get(sse_handler))
        .route("/api/records", get(records_handler))
        // Agent read path — bounded, deduplicated, cursor-based.
        .route("/api/query", get(query_handler))
        .route("/api/errors", get(errors_handler))
        .route("/api/sources", get(sources_handler))
        .route("/api/wait", get(wait_handler))
        .route("/health", get(health_handler))
        // Access control (M4-A): sign-in link, session probe, sign-in link
        // minting (token holders only), sign-out.
        .route("/auth/login/{code}", get(login_handler))
        .route("/api/v1/auth/session", get(session_handler))
        .route("/api/v1/auth/login-links", post(mint_login_link_handler))
        .route("/api/v1/auth/logout", post(logout_handler))
        .route(
            "/api/v1/auth/sessions/revoke",
            post(revoke_sessions_handler),
        )
        .merge(crate::api_v1::router())
        // Everything else → embedded web UI
        .fallback(static_handler)
        // Innermost: runs after the Host and Origin checks below.
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(guard_browser_origin))
        .layer(middleware::from_fn(validate_local_host))
        .with_state(state)
}

/// No endpoint accepts a body; anything larger than this is refused unread.
const MAX_BODY_BYTES: usize = 16 * 1024;
/// Longest accepted request target. Query strings beyond this are not a
/// legitimate filter.
const MAX_URI_BYTES: usize = 8 * 1024;

fn is_loopback_origin(origin: &str) -> bool {
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let (host, port) = match authority.strip_prefix("[::1]") {
        Some(rest) => ("[::1]", rest),
        None => authority
            .find(':')
            .map_or((authority, ""), |i| authority.split_at(i)),
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    port_ok && matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Browser-facing protections, layered on the Host check:
///
/// - A request carrying an `Origin` that is not this machine's loopback is
///   refused (403). Browsers attach `Origin` to cross-origin fetches, so a web
///   page cannot use a visitor's browser to query local logs.
/// - `Sec-Fetch-Site: cross-site` is refused for anything except a top-level
///   navigation (a user clicking a link to the dashboard).
/// - Oversized request targets are refused (414).
///
/// Non-browser clients (curl, `tailflow-mcp`) send neither header and are
/// unaffected.
async fn guard_browser_origin(request: Request, next: Next) -> Response {
    if request.uri().to_string().len() > MAX_URI_BYTES {
        return (StatusCode::URI_TOO_LONG, "request target too long").into_response();
    }
    let headers = request.headers();
    if let Some(origin) = headers.get(header::ORIGIN) {
        let ok = origin.to_str().is_ok_and(is_loopback_origin);
        if !ok {
            return (
                StatusCode::FORBIDDEN,
                "TailFlow accepts browser requests from loopback origins only",
            )
                .into_response();
        }
    }
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    let mode = headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok());
    if site == Some("cross-site") && mode != Some("navigate") {
        return (
            StatusCode::FORBIDDEN,
            "cross-site browser requests are not accepted",
        )
            .into_response();
    }
    next.run(request).await
}

async fn validate_local_host(request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|host| {
            host == "localhost"
                || host.starts_with("localhost:")
                || host == "127.0.0.1"
                || host.starts_with("127.0.0.1:")
                || host == "[::1]"
                || host.starts_with("[::1]:")
        });
    if !allowed {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "TailFlow accepts loopback Host headers only",
        )
            .into_response();
    }
    next.run(request).await
}

// ── Access control ────────────────────────────────────────────────────────────

/// The credential that authorized a request (`None` on public paths reached
/// without one). Inserted by [`require_auth`] on every request.
#[derive(Clone, Copy)]
pub(crate) struct AuthOutcome(pub(crate) Option<Credential>);

/// Paths served without a credential: the static dashboard shell (it holds
/// no data), liveness, and the sign-in link itself. Everything else —
/// including any endpoint added later and unknown paths — requires one, so a
/// new route cannot ship unauthenticated by accident.
fn is_public(path: &str) -> bool {
    if path == "/" || path == "/health" {
        return true;
    }
    if let Some(code) = path.strip_prefix("/auth/login/") {
        return !code.is_empty() && !code.contains('/');
    }
    let asset = path.trim_start_matches('/');
    !asset.is_empty() && !asset.starts_with("api/") && WebAssets::get(asset).is_some()
}

async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let outcome = state.auth.authorize(request.headers());
    if is_public(request.uri().path()) {
        request.extensions_mut().insert(AuthOutcome(outcome.ok()));
        return next.run(request).await;
    }
    match outcome {
        Ok(credential) => {
            request
                .extensions_mut()
                .insert(AuthOutcome(Some(credential)));
            next.run(request).await
        }
        Err(denied) => {
            state.auth.note_refused();
            denied_response(denied)
        }
    }
}

/// The same answer for every protected path, existing or not: a refused
/// caller cannot probe which sessions, events or files exist.
fn denied_response(denied: Denied) -> Response {
    let status = match denied {
        Denied::CrossOrigin => StatusCode::FORBIDDEN,
        Denied::Missing | Denied::BadToken | Denied::SessionExpired => StatusCode::UNAUTHORIZED,
    };
    let mut response = (
        status,
        Json(serde_json::json!({ "error": denied.message(), "kind": denied.kind() })),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"tailflow\""),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Longest sign-in code accepted (codes are 64 hex characters).
const MAX_CODE_LEN: usize = 128;

/// `GET /auth/login/<code>`: redeem a one-time sign-in code for a session
/// cookie, then redirect to the dashboard. The code is dead after this
/// request, so its appearance in browser history is harmless; the redirect
/// keeps it out of the address bar and `Referrer-Policy: no-referrer` out
/// of any request the dashboard makes.
async fn login_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(code): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Only a link the user opened (`none`: terminal, address bar) or our own
    // page. A page on another localhost port navigating here is `same-site`.
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    if !matches!(site, None | Some("none" | "same-origin")) {
        state.auth.note_refused();
        return login_page(
            StatusCode::FORBIDDEN,
            "This sign-in link was opened by another web page, so it was not used. \
             Open it directly from your terminal instead.",
        );
    }
    let redeemed = if code.len() <= MAX_CODE_LEN {
        state.auth.redeem_code(&code)
    } else {
        Err(())
    };
    let Ok(session) = redeemed else {
        state.auth.note_refused();
        return login_page(
            StatusCode::UNAUTHORIZED,
            "This sign-in link has expired or was already used. Run \
             `tailflow-daemon dashboard` for a new one.",
        );
    };
    let cookie = crate::auth::session_cookie(&state.auth.cookie_name(), &session);
    let mut response = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::REFERRER_POLICY, "no-referrer");
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        response = response.header(header::SET_COOKIE, v);
    }
    response
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// A plain page (no script, no styles) for a refused sign-in link.
fn login_page(status: StatusCode, message: &str) -> Response {
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>TailFlow sign-in</title></head><body><h1>TailFlow</h1><p>{}</p>\
         <p><a href=\"/\">Open the dashboard</a></p></body></html>",
        html_escape(message)
    );
    let mut response = (status, Html(body)).into_response();
    let headers = response.headers_mut();
    for (name, value) in UI_SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `GET /api/v1/auth/session`: which credential this request carries. The
/// dashboard calls it to decide between the app and the sign-in screen.
async fn session_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(outcome): axum::Extension<AuthOutcome>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "authenticated": outcome.0.is_some(),
        "credential": outcome.0,
        "instance_id": state.instance_id,
        "session_idle_timeout_secs": crate::auth::SESSION_IDLE.as_secs(),
    }))
}

/// `POST /api/v1/auth/login-links`: mint a one-time sign-in link. API token
/// only — a browser session cannot mint links, so a session never becomes
/// more than it was.
async fn mint_login_link_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(outcome): axum::Extension<AuthOutcome>,
) -> Response {
    if outcome.0 != Some(Credential::Token) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "sign-in links can be created only with the API token",
                "kind": "token_required",
            })),
        )
            .into_response();
    }
    match state.auth.mint_code(crate::auth::MINTED_CODE_TTL) {
        Ok((code, _)) => {
            let mut response = Json(serde_json::json!({
                "path": format!("/auth/login/{code}"),
                "expires_in_secs": crate::auth::MINTED_CODE_TTL.as_secs(),
                "single_use": true,
            }))
            .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(e) => {
            tracing::error!(err = %e, "could not mint a sign-in code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "no random source", "kind": "internal" })),
            )
                .into_response()
        }
    }
}

/// `POST /api/v1/auth/sessions/revoke`: end every browser session. API
/// token only (`tailflow-daemon dashboard --revoke-sessions`).
async fn revoke_sessions_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(outcome): axum::Extension<AuthOutcome>,
) -> Response {
    if outcome.0 != Some(Credential::Token) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "sessions can be revoked only with the API token",
                "kind": "token_required",
            })),
        )
            .into_response();
    }
    let revoked = state.auth.revoke_all_sessions();
    Json(serde_json::json!({ "revoked": revoked })).into_response()
}

/// `POST /api/v1/auth/logout`: end this browser's session.
async fn logout_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    for value in state.auth.session_cookies(&headers) {
        let _was_ours = state.auth.end_session(&value);
    }
    let mut response = Json(serde_json::json!({ "signed_out": true })).into_response();
    if let Ok(v) = HeaderValue::from_str(&crate::auth::clear_cookie(&state.auth.cookie_name())) {
        response.headers_mut().insert(header::SET_COOKIE, v);
    }
    response
}

// ── Shared filter params ──────────────────────────────────────────────────────

/// Query parameters accepted by `/events` and `/api/records`.
///
/// Examples:
///   GET /events?grep=error
///   GET /api/records?source=nginx
///   GET /events?grep=panic&source=api
#[derive(Debug, Deserialize, Default)]
struct FilterParams {
    /// Regex matched against `record.payload`.
    grep: Option<String>,
    /// Substring matched against `record.source`.
    source: Option<String>,
    /// `/api/records` only: records with a ring seq above this. Lets a
    /// reconnecting dashboard catch up without re-reading the whole ring.
    after: Option<u64>,
}

impl FilterParams {
    fn into_filter(self) -> Filter {
        let f = match self.grep.as_deref() {
            Some(pat) => Filter::regex(pat).unwrap_or_else(|e| {
                tracing::warn!(pattern = pat, err = %e, "invalid grep regex, ignoring");
                Filter::none()
            }),
            None => Filter::none(),
        };
        match self.source {
            Some(src) => f.with_source(src),
            None => f,
        }
    }
}

// ── SSE ───────────────────────────────────────────────────────────────────────

async fn sse_handler(
    State(state): State<Arc<AppState>>,
    AxumQuery(params): AxumQuery<FilterParams>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, axum::Error>>> {
    let filter = params.into_filter();
    let rx = state.tx.subscribe();
    let ingest = state.ingest.clone();
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        Ok(record) if filter.matches_seq(&record) => match serde_json::to_string(&*record) {
            Ok(data) => Some(Ok(Event::default().data(data))),
            Err(e) => {
                tracing::error!(err = %e, "failed to serialize log record for SSE");
                None
            }
        },
        Ok(_) => None,
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!(dropped = n, "SSE client lagged");
            ingest
                .sse_lagged
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            // A named event: clients that only listen to `message` (older
            // dashboards) ignore it; current ones rehydrate from the ring.
            Some(Ok(Event::default()
                .event("gap")
                .data(serde_json::json!({ "dropped": n }).to_string())))
        }
    });

    Sse::new(EndOnShutdown::new(stream, state.shutdown.clone())).keep_alive(KeepAlive::default())
}

/// A stream that ends as soon as the daemon starts shutting down.
///
/// The live record stream never ends by itself (the broadcast sender lives
/// as long as the server), and the HTTP server's graceful shutdown waits for
/// every open connection. Without this, one connected dashboard tab or SSE
/// client kept Ctrl-C / SIGTERM from ever stopping the daemon, so storage
/// never drained and a forced kill was recorded as an unclean shutdown.
struct EndOnShutdown<S> {
    inner: std::pin::Pin<Box<S>>,
    cancelled: std::pin::Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
}

impl<S> EndOnShutdown<S> {
    fn new(inner: S, shutdown: tokio_util::sync::CancellationToken) -> Self {
        Self {
            inner: Box::pin(inner),
            cancelled: Box::pin(shutdown.cancelled_owned()),
        }
    }
}

impl<S: tokio_stream::Stream> tokio_stream::Stream for EndOnShutdown<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<S::Item>> {
        if std::future::Future::poll(self.cancelled.as_mut(), cx).is_ready() {
            return std::task::Poll::Ready(None);
        }
        self.inner.as_mut().poll_next(cx)
    }
}

// ── REST ──────────────────────────────────────────────────────────────────────

async fn records_handler(
    State(state): State<Arc<AppState>>,
    AxumQuery(params): AxumQuery<FilterParams>,
) -> impl IntoResponse {
    // Human-facing: no elision, no dedup — the dashboard renders raw lines.
    // Serialized straight from the shared ring entries: no per-request copy
    // of every retained payload.
    let after = params.after;
    let mut records = state.store.snapshot(&params.into_filter());
    if let Some(after) = after {
        records.retain(|r| r.seq > after);
    }
    Json(SharedRecords(records))
}

/// Serializes shared ring entries exactly as `Vec<SeqRecord>` would.
struct SharedRecords(Vec<Arc<tailflow_core::query::SeqRecord>>);

impl serde::Serialize for SharedRecords {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(AsRef::as_ref))
    }
}

async fn health_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(outcome): axum::Extension<AuthOutcome>,
) -> Response {
    if outcome.0.is_none() {
        // Liveness for anyone (supervisors, `curl`), and nothing else: no
        // counters, ids or storage state without a credential.
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
                "auth_required": true,
            })),
        )
            .into_response();
    }
    let ingest = state.ingest.view();
    let storage = match &state.storage {
        crate::state::StorageSlot::Enabled(s) => {
            let h = s.health();
            serde_json::json!({
                "state": h.writer_state,
                "loss_detected": h.loss_detected,
                "events_persisted": h.events_persisted,
                "events_dropped": h.events_dropped_total,
            })
        }
        crate::state::StorageSlot::Disabled => serde_json::json!({ "state": "disabled" }),
        crate::state::StorageSlot::Unavailable { kind, .. } => {
            serde_json::json!({ "state": "unavailable", "kind": kind })
        }
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            // `ok` means the daemon is serving. Check `ingest_dropped` and
            // `storage.loss_detected` before treating history as complete.
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "instance_id": state.instance_id,
            "buffered": state.store.len(),
            "buffered_bytes": state.store.retained_bytes(),
            "cursor": state.store.cursor(),
            "ingest_dropped": ingest.records_dropped,
            "storage": storage,
            "auth": state.auth.view(),
            "agent_stream": state.agent_feed.as_ref().map(|f| f.stats()),
        })),
    )
        .into_response()
}

/// Add the live-ingestion loss counter to a legacy response. Additive: older
/// clients ignore the field; agents use it to avoid reading an incomplete
/// buffer as "nothing happened".
fn with_ingest_loss(state: &AppState, mut v: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "ingest_dropped".into(),
            serde_json::Value::from(
                state
                    .ingest
                    .dropped
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
        );
    }
    v
}

// ── Agent read path ───────────────────────────────────────────────────────────

/// Query parameters shared by the agent endpoints.
///
/// Unlike [`FilterParams`], a malformed value here is a **400**, not a silently
/// dropped filter. A caller that cannot see the screen has no way to notice
/// that its regex was ignored — it would read the empty result as "no errors"
/// and report success. Failing loudly is the only safe default.
#[derive(Debug, Deserialize, Default)]
struct AgentParams {
    grep: Option<String>,
    source: Option<String>,
    /// Minimum severity: `trace` | `debug` | `info` | `warn` | `error`.
    level: Option<String>,
    /// RFC 3339 instant or a relative duration (`30s`, `5m`, `2h`, `1d`).
    since: Option<String>,
    limit: Option<usize>,
    /// Return only records newer than this sequence number.
    cursor: Option<u64>,
    max_payload_chars: Option<usize>,
    /// Trailing continuation lines attached to each error group.
    context_lines: Option<usize>,
    timeout_ms: Option<u64>,
    /// Wait only for records that arrive *after* the call starts, ignoring any
    /// match already held in the buffer.
    require_new: Option<bool>,
}

/// Upper bounds. A caller can ask for less, never more — one runaway `limit`
/// must not be able to serialise the whole buffer into someone's context.
const MAX_LIMIT: usize = 1_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_CONTEXT_LINES: usize = 50;
const MAX_PAYLOAD_CHARS: usize = 10_000;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

/// [`AgentParams`], but with a rejection that matches the rest of the agent
/// contract.
///
/// Axum's own `Query` rejection is a plain-text body. Every other bad argument
/// on these endpoints answers with `{"error": "..."}`, and a caller that reads
/// that field would find nothing at all in a plain-text body — the same silent
/// nothing it gets from a successful empty result. A malformed `limit=abc` has
/// to be as loud as a malformed `grep`.
struct AgentQuery(AgentParams);

impl<S: Send + Sync> FromRequestParts<S> for AgentQuery {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match AxumQuery::<AgentParams>::from_request_parts(parts, state).await {
            Ok(AxumQuery(params)) => Ok(Self(params)),
            Err(rejection) => Err(ApiError(
                StatusCode::BAD_REQUEST,
                describe_query_rejection(&rejection),
            )),
        }
    }
}

fn describe_query_rejection(rejection: &QueryRejection) -> String {
    format!("invalid query parameters: {}", rejection.body_text())
}

impl AgentParams {
    fn build_filter(&self) -> Result<Filter, ApiError> {
        let mut filter = match self.grep.as_deref() {
            Some(pat) => Filter::regex(pat).map_err(|e| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid `grep` regex: {e}"),
                )
            })?,
            None => Filter::none(),
        };
        if let Some(src) = &self.source {
            filter = filter.with_source(src.clone());
        }
        if let Some(lvl) = &self.level {
            let parsed = LogLevel::parse(lvl).ok_or_else(|| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid `level`: {lvl:?} (expected trace|debug|info|warn|error)"),
                )
            })?;
            filter = filter.with_min_level(parsed);
        }
        if let Some(since) = &self.since {
            let parsed = query::parse_since(since).ok_or_else(|| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid `since`: {since:?} (expected RFC 3339 or 30s|5m|2h|1d)"),
                )
            })?;
            filter = filter.with_since(parsed);
        }
        Ok(filter)
    }

    fn build_query(&self, default_limit: usize) -> Result<Query, ApiError> {
        // Clamping *down* is safe here because the response says so: an over-large
        // `limit` still reports `total_matching` and `truncated`, and an over-large
        // `max_payload_chars` still marks each elided line. Zero is different —
        // there is no "0 records, truncated" to report, so a silently-raised 0
        // would look like a deliberate one-record answer. Reject it.
        let limit = match self.limit {
            Some(0) => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid `limit`: 0 (expected 1..={MAX_LIMIT})"),
                ))
            }
            Some(n) => n.min(MAX_LIMIT),
            None => default_limit,
        };
        let max_payload_chars = match self.max_payload_chars {
            Some(0) => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid `max_payload_chars`: 0 (expected 1..={MAX_PAYLOAD_CHARS})"),
                ))
            }
            Some(n) => n.min(MAX_PAYLOAD_CHARS),
            None => DEFAULT_MAX_PAYLOAD_CHARS,
        };
        Ok(Query::new(self.build_filter()?)
            .with_limit(limit)
            .with_cursor(self.cursor)
            .with_max_payload_chars(max_payload_chars))
    }

    /// Trailing context lines per error group.
    ///
    /// Unlike `limit`, a reduced value here is invisible in the response — a
    /// group carrying 50 of the 200 requested stack frames looks exactly like a
    /// 50-frame stack trace. What cannot be announced must be refused.
    fn context_lines(&self) -> Result<usize, ApiError> {
        match self.context_lines {
            Some(n) if n > MAX_CONTEXT_LINES => Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("invalid `context_lines`: {n} (expected 0..={MAX_CONTEXT_LINES})"),
            )),
            Some(n) => Ok(n),
            None => Ok(DEFAULT_CONTEXT_LINES),
        }
    }
}

/// Raw records, newest-last, with a cursor for incremental follow-up reads.
async fn query_handler(
    State(state): State<Arc<AppState>>,
    AgentQuery(params): AgentQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let q = params.build_query(100)?;
    let v = serde_json::to_value(state.store.records(&q))
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(with_ingest_loss(&state, v)))
}

/// Distinct failures rather than every occurrence — the endpoint an agent
/// should reach for first. Defaults to `level >= error`.
async fn errors_handler(
    State(state): State<Arc<AppState>>,
    AgentQuery(params): AgentQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut params = params;
    params.level.get_or_insert_with(|| "error".to_string());
    let context_lines = params.context_lines()?;
    let q = params.build_query(20)?;
    let summary = state.store.summarize(&q, context_lines);
    let v = serde_json::to_value(summary)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(with_ingest_loss(&state, v)))
}

async fn sources_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "sources": state.source_views(),
        "buffered": state.store.len(),
        "cursor": state.store.cursor(),
        "ingest_dropped": state.ingest.dropped.load(std::sync::atomic::Ordering::Relaxed),
    }))
}

/// Block until a matching record arrives, or `timeout_ms` elapses.
///
/// This is what replaces sleep-and-poll. A caller that just triggered a rebuild
/// asks "tell me when something matching `error` shows up" and is unblocked the
/// instant it does — no fixed sleep that is either too short to catch the
/// failure or too long to be worth waiting for.
async fn wait_handler(
    State(state): State<Arc<AppState>>,
    AgentQuery(params): AgentQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let timeout_ms = params.timeout_ms.unwrap_or(30_000).min(MAX_TIMEOUT_MS);
    let q = params.build_query(100)?;
    let started = Instant::now();

    // Subscribe *before* checking the buffer, so a record landing between the
    // two is caught by the subscription instead of falling through the gap.
    let mut rx = state.tx.subscribe();

    let snapshot = state.store.cursor();

    // `require_new` turns the wait into a strictly forward-looking one. It is the
    // answer to "did my change cause this?" when the caller holds no baseline
    // cursor and cannot guess a `since` window: the buffer is not consulted at
    // all, so a pre-existing match cannot end the wait early.
    let require_new = params.require_new.unwrap_or(false);
    if let Some(anchor) = (!require_new)
        .then(|| state.store.latest_matching_seq(&q))
        .flatten()
    {
        // This match was already in the buffer before the call — it is the
        // previous build's "compiled successfully", not this one's, unless the
        // caller scoped the search with `cursor` or `since`. Answering it is
        // right (the event the caller waited for may genuinely have landed
        // first), but reporting it as a fresh match is not.
        let result = state
            .store
            .records_from(anchor, q.limit, q.max_payload_chars);
        return Ok(Json(with_ingest_loss(&state, wait_result(result, 0, true))));
    }

    let deadline = Duration::from_millis(timeout_ms);
    let shutdown = state.shutdown.clone();
    let waited = tokio::time::timeout(deadline, async {
        loop {
            let received = tokio::select! {
                // The daemon is stopping: end the wait now (graceful shutdown
                // waits for open connections) and say so — an empty
                // "matched: false" would read as "nothing happened".
                () = shutdown.cancelled() => return Err(()),
                r = rx.recv() => r,
            };
            match received {
                Ok(record)
                    if record.seq > snapshot
                        && q.cursor.is_none_or(|cursor| record.seq > cursor)
                        && q.filter.matches_seq(&record) =>
                {
                    return Ok(Some(record.seq))
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "wait subscriber lagged");
                    // Recover from the store, but only forward of the snapshot:
                    // `latest_matching_seq` would happily anchor to a record that
                    // predates the call, which is the one thing a wait must not
                    // report as a new event.
                    if let Some(anchor) = state.store.first_matching_seq_after(&q, snapshot) {
                        return Ok(Some(anchor));
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    })
    .await;
    let matched_seq = match waited {
        Ok(Err(())) => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the daemon is shutting down; the wait was interrupted".into(),
            ))
        }
        Ok(Ok(seq)) => seq,
        Err(_elapsed) => None,
    };

    let Some(anchor) = matched_seq else {
        let horizon = state.store.records(&q);
        return Ok(Json(with_ingest_loss(
            &state,
            serde_json::json!({
                "matched": false,
                "matched_from_buffer": false,
                "records": [],
                "total_matching": 0,
                "truncated": false,
                "next_cursor": horizon.next_cursor,
                "buffer_start_cursor": horizon.buffer_start_cursor,
                "cursor_gap": horizon.cursor_gap,
                "waited_ms": started.elapsed().as_millis() as u64,
            }),
        )));
    };

    // A failure rarely arrives as one line. Let the rest of the burst — the
    // stack trace, the retry, the cascading downstream error — land before
    // answering, so the caller gets the whole event in one round trip.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let result = state
        .store
        .records_from(anchor, q.limit, q.max_payload_chars);
    Ok(Json(with_ingest_loss(
        &state,
        wait_result(result, started.elapsed().as_millis() as u64, false),
    )))
}

/// `from_buffer` separates "this happened while you waited" from "this was
/// already here when you asked". Both are `matched: true`, and only one of them
/// is evidence about the change the caller just made.
fn wait_result(
    result: tailflow_core::query::QueryResult,
    waited_ms: u64,
    from_buffer: bool,
) -> serde_json::Value {
    serde_json::json!({
        "matched": true,
        "matched_from_buffer": from_buffer,
        "records": result.records,
        "total_matching": result.total_matching,
        "truncated": result.truncated,
        "next_cursor": result.next_cursor,
        "buffer_start_cursor": result.buffer_start_cursor,
        "cursor_gap": result.cursor_gap,
        "waited_ms": waited_ms,
    })
}

// ── Static file server ────────────────────────────────────────────────────────

async fn static_handler(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');

    // Default to index.html for the root
    let path = if raw.is_empty() { "index.html" } else { raw };

    let mut response = match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            ([(header::CONTENT_TYPE, mime)], content.data.into_owned()).into_response()
        }
        // Unknown path → serve index.html (SPA client-side routing)
        None => match WebAssets::get("index.html") {
            Some(content) => Html(content.data.into_owned()).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                "Web UI not built — run: cd web && npm install && npm run build",
            )
                .into_response(),
        },
    };
    let headers = response.headers_mut();
    for (name, value) in UI_SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Headers on every UI response. The dashboard renders untrusted text (log
/// lines, agent metadata): the CSP admits only this origin's own scripts
/// and styles, so an injection bug cannot run code or load anything remote,
/// and the page cannot be framed by another site.
const UI_SECURITY_HEADERS: [(header::HeaderName, &str); 4] = [
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; font-src 'self'; manifest-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'",
    ),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::X_FRAME_OPTIONS, "DENY"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use chrono::Utc;
    use http_body_util::BodyExt;
    use tailflow_core::LogRecord;
    use tower::ServiceExt;

    fn state_with(records: Vec<(&str, LogLevel, &str)>) -> Arc<AppState> {
        let (_source_tx, source_rx) = tailflow_core::new_bus();
        let state = AppState::new(source_rx, 1000);
        for (source, level, payload) in records {
            state.store.push(LogRecord {
                timestamp: Utc::now(),
                source: source.into(),
                level,
                payload: payload.into(),
            });
        }
        state
    }

    /// `Authorization: Bearer <this run's token>`, as a local CLI sends it.
    fn bearer(state: &AppState) -> String {
        format!("Bearer {}", state.auth.token().expose())
    }

    async fn get(state: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
        let auth = bearer(&state);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn flood() -> Arc<AppState> {
        let mut records = vec![("web", LogLevel::Info, "compiled successfully")];
        for _ in 0..30 {
            records.push((
                "api",
                LogLevel::Error,
                "ERROR connection refused: postgres:5432",
            ));
        }
        records.push(("api", LogLevel::Unknown, "    at Pool.connect (db.js:42)"));
        state_with(records)
    }

    // ── shutdown ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sse_stream_ends_when_the_daemon_shuts_down() {
        let state = state_with(vec![]);
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .header(header::AUTHORIZATION, bearer(&state))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        state.shutdown.cancel();
        // The body must end; before the fix it never did, and the server's
        // graceful shutdown waited on it forever.
        tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .expect("the SSE body ends on shutdown")
            .expect("and ends cleanly");
    }

    #[tokio::test]
    async fn wait_is_interrupted_by_shutdown_and_says_so() {
        let state = state_with(vec![]);
        let waiting = tokio::spawn(get(
            state.clone(),
            "/api/wait?grep=never&timeout_ms=60000&require_new=true",
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        state.shutdown.cancel();
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the wait ends on shutdown, not at its 60 s timeout")
            .unwrap();
        // Not 200 + "matched: false", which an agent would read as "nothing
        // happened".
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or("")
                .contains("shutting down"),
            "{body}"
        );
    }

    // ── /api/errors ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn errors_deduplicates_and_defaults_to_error_level() {
        let (status, body) = get(flood(), "/api/errors").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["distinct"], 1, "30 identical errors are one failure");
        assert_eq!(body["total_matching"], 30);
        assert_eq!(body["groups"][0]["count"], 30);
        // The info line must not appear: `level` defaults to error.
        assert_eq!(body["groups"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn errors_attaches_trailing_context() {
        let (_, body) = get(flood(), "/api/errors").await;
        let ctx = body["groups"][0]["context"].as_array().unwrap();
        assert_eq!(ctx.len(), 1);
        assert!(ctx[0].as_str().unwrap().contains("Pool.connect"));
    }

    #[tokio::test]
    async fn errors_context_lines_zero_omits_context() {
        let (_, body) = get(flood(), "/api/errors?context_lines=0").await;
        assert!(body["groups"][0].get("context").is_none());
    }

    #[tokio::test]
    async fn errors_level_can_be_widened() {
        let (_, body) = get(flood(), "/api/errors?level=info").await;
        assert_eq!(body["distinct"], 2, "info line now counts as its own group");
    }

    // ── Argument validation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn invalid_regex_is_rejected_not_silently_ignored() {
        let (status, body) = get(flood(), "/api/query?grep=%5B%5B%5Bbad").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("grep"));
    }

    #[tokio::test]
    async fn invalid_level_is_rejected() {
        let (status, body) = get(flood(), "/api/query?level=loud").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("expected"));
    }

    #[tokio::test]
    async fn invalid_since_is_rejected() {
        let (status, body) = get(flood(), "/api/errors?since=yesterday").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("since"));
    }

    #[tokio::test]
    async fn unicode_since_is_rejected_without_panicking() {
        let (status, body) = get(flood(), "/api/query?since=%F0%9F%95%92").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("since"));
    }

    #[tokio::test]
    async fn malformed_numeric_param_is_a_json_error_not_plain_text() {
        // Axum's own rejection is a plain-text body. A caller that reads `error`
        // finds nothing there — the same nothing a clean run produces.
        let (status, body) = get(flood(), "/api/query?limit=abc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "rejection must use the same {{\"error\": ...}} shape: {body}"
        );
    }

    #[tokio::test]
    async fn every_agent_endpoint_reports_a_bad_filter_the_same_way() {
        for path in ["/api/query", "/api/errors", "/api/wait"] {
            let (status, body) = get(flood(), &format!("{path}?grep=%5B%5B%5Bbad")).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{path} accepted a bad regex"
            );
            assert!(body["error"].as_str().unwrap().contains("grep"), "{path}");
        }
    }

    #[tokio::test]
    async fn wait_rejects_a_bad_filter_instead_of_waiting_out_the_timeout() {
        // Waiting the full timeout and answering "nothing matched" would be the
        // worst possible answer: a confident negative from a filter that never ran.
        let (status, body) = get(flood(), "/api/wait?level=loud&timeout_ms=120000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("level"));
    }

    #[tokio::test]
    async fn zero_valued_bounds_are_rejected_not_silently_raised() {
        // There is no "0 records, truncated" to report, so a 0 quietly raised to 1
        // would return one record and look like a deliberate answer.
        let (status, body) = get(flood(), "/api/query?limit=0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("limit"));

        let (status, body) = get(flood(), "/api/query?max_payload_chars=0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("max_payload_chars"));
    }

    #[tokio::test]
    async fn over_max_context_lines_is_rejected_because_the_cut_is_invisible() {
        // A group holding 50 of the 200 frames asked for is indistinguishable from
        // a 50-frame stack trace, so this bound is refused rather than clamped.
        let (status, body) = get(flood(), "/api/errors?context_lines=200").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"].as_str().unwrap();
        assert!(message.contains("context_lines"), "{message}");
        assert!(
            message.contains(&MAX_CONTEXT_LINES.to_string()),
            "must state the bound: {message}"
        );

        let (status, _) = get(
            flood(),
            &format!("/api/errors?context_lines={MAX_CONTEXT_LINES}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the maximum itself must be allowed");
    }

    #[tokio::test]
    async fn errors_reports_group_truncation_rather_than_implying_completeness() {
        // Distinct *words*, not distinct numbers: fingerprinting normalises
        // numbers away on purpose, so numbered payloads would collapse to one
        // group and prove nothing about the group limit.
        let payloads: Vec<String> = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliett", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
            "sierra", "tango", "uniform", "victor", "whiskey", "xray", "yankee",
        ]
        .iter()
        .map(|name| format!("ERROR the {name} handler refused to start"))
        .collect();
        let records: Vec<_> = payloads
            .iter()
            .map(|payload| ("api", LogLevel::Error, payload.as_str()))
            .collect();
        let (status, body) = get(state_with(records), "/api/errors").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["distinct"], 25, "all distinct failures are counted");
        assert_eq!(
            body["groups"].as_array().unwrap().len(),
            20,
            "default limit caps the groups returned"
        );
        assert_eq!(body["truncated"], true, "and the response says so");
    }

    #[tokio::test]
    async fn every_bound_that_is_clamped_is_visible_in_the_response() {
        // The rule: a bound may be silently reduced only when the response
        // announces the reduction. `limit` announces via `truncated`, and
        // `max_payload_chars` via `payload_truncated_from`.
        let (_, body) = get(flood(), "/api/query?level=error&limit=99999999").await;
        assert_eq!(body["total_matching"], 30);
        assert!(body["truncated"].is_boolean());

        let state = state_with(vec![("api", LogLevel::Info, &"x".repeat(20_000))]);
        let (_, body) = get(state, "/api/query?max_payload_chars=99999999").await;
        assert_eq!(body["records"][0]["payload_truncated_from"], 20_000);
    }

    // ── /api/query ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn query_limit_is_clamped_and_reports_truncation() {
        let (status, body) = get(flood(), "/api/query?level=error&limit=5").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().unwrap().len(), 5);
        assert_eq!(body["total_matching"], 30);
        assert_eq!(body["truncated"], true);

        // An absurd limit must clamp rather than overflow or serialise the world.
        let (status, body) = get(flood(), "/api/query?limit=99999999").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["records"].as_array().unwrap().len() <= MAX_LIMIT);
    }

    #[tokio::test]
    async fn query_cursor_returns_only_newer_records() {
        let state = flood();
        let (_, all) = get(state.clone(), "/api/query?limit=1000").await;
        let cursor = all["next_cursor"].as_u64().unwrap();

        state.store.push(tailflow_core::LogRecord {
            timestamp: Utc::now(),
            source: "api".into(),
            level: LogLevel::Error,
            payload: "ERROR something new".into(),
        });

        let (_, body) = get(state, &format!("/api/query?cursor={cursor}")).await;
        assert_eq!(body["records"].as_array().unwrap().len(), 1);
        assert_eq!(body["records"][0]["payload"], "ERROR something new");
    }

    #[tokio::test]
    async fn query_elides_huge_payloads_but_records_endpoint_does_not() {
        let state = state_with(vec![("api", LogLevel::Info, &"x".repeat(9_000))]);

        let (_, agent) = get(state.clone(), "/api/query").await;
        let payload = agent["records"][0]["payload"].as_str().unwrap();
        assert!(payload.len() < 9_000, "agent path must bound payload size");
        assert_eq!(agent["records"][0]["payload_truncated_from"], 9_000);

        // The dashboard renders raw lines and must keep seeing them in full.
        let (_, human) = get(state, "/api/records").await;
        assert_eq!(human[0]["payload"].as_str().unwrap().len(), 9_000);
    }

    #[tokio::test]
    async fn query_clamps_requested_payload_limit() {
        let state = state_with(vec![("api", LogLevel::Info, &"x".repeat(12_000))]);
        let (_, body) = get(state, "/api/query?max_payload_chars=999999999").await;
        let payload = body["records"][0]["payload"].as_str().unwrap();
        assert_eq!(
            payload.chars().take(MAX_PAYLOAD_CHARS).count(),
            MAX_PAYLOAD_CHARS
        );
        assert!(payload.ends_with("… [+2000 chars]"));
        assert!(payload.chars().count() < 12_000);
        assert_eq!(body["records"][0]["payload_truncated_from"], 12_000);
    }

    // ── /api/sources ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sources_reports_per_service_counters() {
        let (status, body) = get(flood(), "/api/sources").await;
        assert_eq!(status, StatusCode::OK);
        let sources = body["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0]["name"], "api", "noisiest source first");
        assert_eq!(sources[0]["errors"], 30);
        assert_eq!(body["buffered"], 32);
    }

    // ── /api/wait ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_returns_immediately_when_a_match_already_exists() {
        let (status, body) = get(flood(), "/api/wait?level=error&timeout_ms=60000").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["matched"], true);
        assert_eq!(body["waited_ms"], 0, "must not block on an existing match");
    }

    #[tokio::test]
    async fn wait_marks_a_pre_existing_match_as_coming_from_the_buffer() {
        // The dangerous case: the buffer holds the *previous* build's success
        // line. Answering it is fine; presenting it as a fresh event is not.
        let (_, body) = get(flood(), "/api/wait?grep=compiled&timeout_ms=60000").await;
        assert_eq!(body["matched"], true);
        assert_eq!(body["waited_ms"], 0);
        assert_eq!(
            body["matched_from_buffer"], true,
            "a match older than the call must say so"
        );
    }

    #[tokio::test]
    async fn wait_marks_a_live_match_as_not_from_the_buffer() {
        let state = state_with(vec![]);
        let publisher = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let record = publisher.store.push(LogRecord {
                timestamp: Utc::now(),
                source: "web".into(),
                level: LogLevel::Info,
                payload: "compiled successfully".into(),
            });
            let _ = publisher.tx.send(record);
        });
        let (_, body) = get(state, "/api/wait?grep=compiled&timeout_ms=5000").await;
        assert_eq!(body["matched"], true);
        assert_eq!(body["matched_from_buffer"], false);
    }

    #[tokio::test]
    async fn wait_since_excludes_a_stale_buffer_match() {
        // `since` is what lets a caller demand a *new* event rather than accept
        // whatever the buffer already held.
        let state = state_with(vec![]);
        state.store.push(LogRecord {
            timestamp: Utc::now() - chrono::Duration::minutes(30),
            source: "web".into(),
            level: LogLevel::Info,
            payload: "compiled successfully".into(),
        });

        let (_, body) = get(state.clone(), "/api/wait?grep=compiled&timeout_ms=100").await;
        assert_eq!(body["matched"], true, "unbounded wait accepts the old line");

        let (status, body) = get(state, "/api/wait?grep=compiled&since=5m&timeout_ms=100").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["matched"], false,
            "a 30-minute-old line must not satisfy `since=5m`"
        );
    }

    #[tokio::test]
    async fn require_new_refuses_to_answer_from_the_buffer() {
        // Without it, the previous build's line ends the wait instantly.
        let (_, body) = get(flood(), "/api/wait?grep=compiled&timeout_ms=100").await;
        assert_eq!(body["matched"], true);
        assert_eq!(body["matched_from_buffer"], true);

        let (status, body) = get(
            flood(),
            "/api/wait?grep=compiled&require_new=true&timeout_ms=100",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["matched"], false,
            "a line older than the call is not a new event"
        );
        assert!(
            body["waited_ms"].as_u64().unwrap() >= 100,
            "must actually wait"
        );
    }

    #[tokio::test]
    async fn require_new_still_matches_a_line_that_arrives_during_the_wait() {
        let state = flood(); // already holds a matching "compiled successfully"
        let publisher = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let record = publisher.store.push(LogRecord {
                timestamp: Utc::now(),
                source: "web".into(),
                level: LogLevel::Info,
                payload: "compiled successfully in 412ms".into(),
            });
            let _ = publisher.tx.send(record);
        });

        let (_, body) = get(
            state,
            "/api/wait?grep=compiled&require_new=true&timeout_ms=5000",
        )
        .await;
        assert_eq!(body["matched"], true);
        assert_eq!(body["matched_from_buffer"], false);
        assert!(body["records"][0]["payload"]
            .as_str()
            .unwrap()
            .contains("412ms"));
    }

    #[tokio::test]
    async fn require_new_rejects_a_non_boolean_value() {
        let (status, body) = get(flood(), "/api/wait?require_new=yes%20please").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));
    }

    #[tokio::test]
    async fn wait_timeout_is_never_reported_as_a_buffer_match() {
        let (_, body) = get(flood(), "/api/wait?grep=never-appears&timeout_ms=100").await;
        assert_eq!(body["matched"], false);
        assert_eq!(body["matched_from_buffer"], false);
    }

    #[tokio::test]
    async fn wait_times_out_without_claiming_a_match() {
        let (status, body) = get(flood(), "/api/wait?grep=never-appears&timeout_ms=150").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["matched"], false);
        assert_eq!(body["records"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn wait_timeout_reports_an_invalid_cursor_horizon() {
        let (_, body) = get(flood(), "/api/wait?cursor=999999&timeout_ms=1").await;
        assert_eq!(body["matched"], false);
        assert_eq!(body["cursor_gap"], true);
        assert!(body["buffer_start_cursor"].as_u64().is_some());
    }

    #[tokio::test]
    async fn wait_wakes_on_a_record_published_after_the_call_starts() {
        let state = state_with(vec![]);
        let publisher = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let record = tailflow_core::LogRecord {
                timestamp: Utc::now(),
                source: "api".into(),
                level: LogLevel::Error,
                payload: "ERROR deploy failed".into(),
            };
            let record = publisher.store.push(record);
            let _ = publisher.tx.send(record);
        });

        let (_, body) = get(state, "/api/wait?grep=deploy%20failed&timeout_ms=5000").await;
        assert_eq!(body["matched"], true);
        assert_eq!(body["records"][0]["payload"], "ERROR deploy failed");
    }

    // ── /health ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn health_reports_buffer_state() {
        let (status, body) = get(flood(), "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["buffered"], 32);
        assert!(body["cursor"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn wait_returns_unfiltered_burst_after_matching_anchor() {
        let state = state_with(vec![]);
        let publisher = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            for (level, payload) in [
                (LogLevel::Error, "ERROR deploy failed"),
                (LogLevel::Unknown, "    at deploy.js:10:2"),
                (LogLevel::Info, "worker stopped"),
            ] {
                let record = publisher.store.push(LogRecord {
                    timestamp: Utc::now(),
                    source: "api".into(),
                    level,
                    payload: payload.into(),
                });
                let _ = publisher.tx.send(record);
            }
        });
        let (_, body) = get(state, "/api/wait?grep=deploy%20failed&timeout_ms=2000").await;
        let records = body["records"].as_array().unwrap();
        assert_eq!(
            records.len(),
            3,
            "the trigger filter must not remove burst context"
        );
        assert!(records[1]["payload"]
            .as_str()
            .unwrap()
            .contains("deploy.js"));
    }

    #[tokio::test]
    async fn sources_include_configured_sources_that_have_no_records() {
        let state = state_with(vec![]);
        state.register_source("quiet-api");
        state.mark_source_running("quiet-api");
        let (_, body) = get(state, "/api/sources").await;
        assert_eq!(body["sources"][0]["name"], "quiet-api");
        assert_eq!(body["sources"][0]["status"], "running");
        assert_eq!(body["sources"][0]["total"], 0);
    }

    #[tokio::test]
    async fn hostile_host_header_is_rejected_and_cors_is_not_permissive() {
        let response = router(state_with(vec![]))
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::HOST, "attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);

        let response = router(state_with(vec![]))
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::HOST, "127.0.0.1:7878")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    // ── access control (M4-A) ─────────────────────────────────────────────────

    async fn raw(
        state: &Arc<AppState>,
        method: &str,
        uri: &str,
        headers: &[(&str, String)],
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            b = b.header(*k, v.as_str());
        }
        let response = router(state.clone())
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let hdrs = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, hdrs, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Mint a code with the token and redeem it the way a browser does when
    /// the user opens the link. Returns the `name=value` cookie pair.
    async fn browser_session(state: &Arc<AppState>) -> String {
        let (status, _, body) = raw(
            state,
            "POST",
            "/api/v1/auth/login-links",
            &[("authorization", bearer(state))],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let path = v["path"].as_str().unwrap().to_string();
        let (status, headers, _) =
            raw(state, "GET", &path, &[("sec-fetch-site", "none".into())]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/");
        let set = headers[header::SET_COOKIE].to_str().unwrap().to_string();
        assert!(set.contains("HttpOnly"), "{set}");
        assert!(set.contains("SameSite=Strict"), "{set}");
        set.split(';').next().unwrap().to_string()
    }

    const PROTECTED: &[&str] = &[
        "/events",
        "/api/records",
        "/api/query",
        "/api/errors",
        "/api/sources",
        "/api/wait?timeout_ms=1",
        "/api/v1/events",
        "/api/v1/events/1",
        "/api/v1/storage/status",
        "/api/v1/storage/retention",
        "/api/v1/collectors/status",
        "/api/v1/schema",
        "/api/v1/privacy",
        "/api/v1/sessions",
        "/api/v1/sessions/claude_code:does-not-exist",
        "/api/v1/agents/status",
        "/api/v1/stats",
        "/api/v1/files",
        "/api/v1/auth/session",
        "/api/v1/stream/agent",
        "/api/v1/observe/status",
        "/api/v1/evidence/files",
        "/api/v1/sessions/claude-code:x/changes",
        "/api/v1/not-a-route",
        "/some/unknown/path",
    ];

    #[tokio::test]
    async fn every_data_endpoint_requires_a_credential_and_answers_identically() {
        let state = state_with(vec![("api", LogLevel::Error, "secret detail")]);
        let mut first: Option<(StatusCode, String)> = None;
        for uri in PROTECTED {
            let (status, headers, body) = raw(&state, "GET", uri, &[]).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
            assert!(!body.contains("secret detail"), "{uri}");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{uri}");
            // Existing and missing resources are indistinguishable.
            match &first {
                None => first = Some((status, body)),
                Some((s, b)) => assert_eq!((status, &body), (*s, b), "{uri}"),
            }
        }
        // A wrong token is refused everywhere, and counted.
        for uri in PROTECTED {
            let (status, _, body) = raw(
                &state,
                "GET",
                uri,
                &[("authorization", format!("Bearer {}", "0".repeat(64)))],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
            assert!(body.contains("invalid_token"), "{uri}: {body}");
        }
        assert!(state.auth.view().refused_requests >= 2 * PROTECTED.len() as u64);
    }

    #[tokio::test]
    async fn the_ui_shell_and_liveness_are_public_but_reveal_nothing() {
        let state = state_with(vec![("api", LogLevel::Info, "hello")]);
        let (status, _, body) = raw(&state, "GET", "/health", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["auth_required"], true);
        for hidden in ["instance_id", "buffered", "cursor", "storage", "auth"] {
            assert!(v.get(hidden).is_none(), "{hidden} leaked: {body}");
        }
        let (status, _, body) = raw(
            &state,
            "GET",
            "/health",
            &[("authorization", bearer(&state))],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["buffered"], 1);
        assert_eq!(v["auth"]["token_source"], "generated_per_run");
        assert!(
            !body.contains(state.auth.token().expose()),
            "token never echoed"
        );
        // The dashboard shell is served (or the build hint when the UI is
        // not built) without a credential.
        let (status, _, _) = raw(&state, "GET", "/", &[]).await;
        assert_ne!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_sign_in_link_creates_a_same_origin_browser_session() {
        let state = state_with(vec![("api", LogLevel::Info, "hello")]);
        let cookie = browser_session(&state).await;
        assert!(cookie.starts_with("tailflow_session_0="), "{cookie}");
        // Same-origin fetch (what the dashboard sends).
        let (status, _, body) = raw(
            &state,
            "GET",
            "/api/sources",
            &[
                ("cookie", cookie.clone()),
                ("sec-fetch-site", "same-origin".into()),
                ("host", "127.0.0.1:7878".into()),
                ("origin", "http://127.0.0.1:7878".into()),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The session probe says how the request was authorized.
        let (_, _, body) = raw(
            &state,
            "GET",
            "/api/v1/auth/session",
            &[
                ("cookie", cookie.clone()),
                ("sec-fetch-site", "same-origin".into()),
            ],
        )
        .await;
        assert!(body.contains("\"credential\":\"session\""), "{body}");
        // SSE accepts the session too.
        let (status, headers, _) = raw(
            &state,
            "GET",
            "/api/records",
            &[
                ("cookie", cookie.clone()),
                ("sec-fetch-site", "same-origin".into()),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{headers:?}");
    }

    #[tokio::test]
    async fn another_localhost_port_cannot_ride_the_session_cookie() {
        let state = state_with(vec![("api", LogLevel::Error, "secret detail")]);
        let cookie = browser_session(&state).await;
        // The browser attaches the cookie to requests from a page on
        // 127.0.0.1:3000 (cookies ignore ports); Fetch Metadata says
        // `same-site`. fetch(), <img>/<script> (no Origin) and EventSource.
        for (site, origin, mode) in [
            ("same-site", Some("http://127.0.0.1:3000"), "cors"),
            ("same-site", None, "no-cors"),
            ("cross-site", Some("http://evil.localhost:3000"), "cors"),
        ] {
            let mut headers = vec![
                ("cookie", cookie.clone()),
                ("sec-fetch-site", site.to_string()),
                ("sec-fetch-mode", mode.to_string()),
            ];
            if let Some(o) = origin {
                headers.push(("origin", o.to_string()));
            }
            for uri in ["/api/v1/events", "/events", "/api/v1/sessions"] {
                let (status, _, body) = raw(&state, "GET", uri, &headers).await;
                assert!(
                    status == StatusCode::FORBIDDEN,
                    "{uri} site={site} origin={origin:?}: {status} {body}"
                );
                assert!(!body.contains("secret detail"));
            }
        }
        // No Fetch Metadata at all (an old browser, or a replayed cookie).
        let (status, _, _) = raw(&state, "GET", "/api/v1/events", &[("cookie", cookie)]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sign_in_links_are_single_use_and_refused_from_other_pages() {
        let state = state_with(vec![]);
        let (_, _, body) = raw(
            &state,
            "POST",
            "/api/v1/auth/login-links",
            &[("authorization", bearer(&state))],
        )
        .await;
        let path = serde_json::from_str::<serde_json::Value>(&body).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        // A page on another localhost port navigating to the link: refused,
        // and the code is not consumed.
        let (status, headers, page) = raw(
            &state,
            "GET",
            &path,
            &[("sec-fetch-site", "same-site".into())],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
        assert!(headers.get(header::SET_COOKIE).is_none());
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self'"));
        // The user opens it: works once.
        let (status, headers, _) = raw(&state, "GET", &path, &[]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(headers.get(header::SET_COOKIE).is_some());
        let (status, headers, page) = raw(&state, "GET", &path, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(headers.get(header::SET_COOKIE).is_none());
        assert!(page.contains("expired or was already used"), "{page}");
        // Guessing.
        for guess in ["/auth/login/0", &format!("/auth/login/{}", "f".repeat(64))] {
            let (status, _, _) = raw(&state, "GET", guess, &[]).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let long = format!("/auth/login/{}", "a".repeat(4000));
        let (status, _, _) = raw(&state, "GET", &long, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn only_the_api_token_can_mint_sign_in_links() {
        let state = state_with(vec![]);
        let cookie = browser_session(&state).await;
        let (status, _, body) = raw(
            &state,
            "POST",
            "/api/v1/auth/login-links",
            &[("cookie", cookie), ("sec-fetch-site", "same-origin".into())],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("token_required"));
        let (status, _, _) = raw(&state, "POST", "/api/v1/auth/login-links", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // GET does not mint.
        let (status, _, _) = raw(
            &state,
            "GET",
            "/api/v1/auth/login-links",
            &[("authorization", bearer(&state))],
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn signing_out_ends_the_session() {
        let state = state_with(vec![]);
        let cookie = browser_session(&state).await;
        let same_origin = vec![
            ("cookie", cookie.clone()),
            ("sec-fetch-site", "same-origin".to_string()),
        ];
        let (status, headers, _) = raw(&state, "POST", "/api/v1/auth/logout", &same_origin).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        let (status, _, body) = raw(&state, "GET", "/api/v1/events", &same_origin).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("session_expired"), "{body}");
    }

    #[tokio::test]
    async fn revoking_ends_every_browser_session_and_needs_the_token() {
        let state = state_with(vec![]);
        let a = browser_session(&state).await;
        let b = browser_session(&state).await;
        let (status, _, _) = raw(
            &state,
            "POST",
            "/api/v1/auth/sessions/revoke",
            &[
                ("cookie", a.clone()),
                ("sec-fetch-site", "same-origin".into()),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a session cannot revoke");
        let (status, _, body) = raw(
            &state,
            "POST",
            "/api/v1/auth/sessions/revoke",
            &[("authorization", bearer(&state))],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"revoked\":2"), "{body}");
        for c in [a, b] {
            let (status, _, _) = raw(
                &state,
                "GET",
                "/api/v1/events",
                &[("cookie", c), ("sec-fetch-site", "same-origin".into())],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    /// Without a session store (tests, no data directory) nothing survives
    /// a restart; the token never does. See `e2e_auth` for the persistent
    /// session store.
    async fn a_restarted_daemon_rejects_old_credentials() {
        let before = state_with(vec![]);
        let cookie = browser_session(&before).await;
        let old_token = bearer(&before);
        let after = state_with(vec![]);
        let (status, _, body) = raw(
            &after,
            "GET",
            "/api/v1/events",
            &[("cookie", cookie), ("sec-fetch-site", "same-origin".into())],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("session_expired"));
        let (status, _, _) = raw(
            &after,
            "GET",
            "/api/v1/events",
            &[("authorization", old_token)],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn dns_rebinding_is_refused_even_with_a_valid_credential() {
        let state = state_with(vec![]);
        for host in [
            "attacker.example:7878",
            "127.0.0.1.nip.io:7878",
            "localhost.attacker.example",
            "localhost.",
        ] {
            let (status, _, _) = raw(
                &state,
                "GET",
                "/api/v1/events",
                &[("host", host.into()), ("authorization", bearer(&state))],
            )
            .await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{host}");
        }
    }
}
