use crate::state::AppState;
use axum::{
    extract::{Query as AxumQuery, Request, State},
    http::{header, StatusCode, Uri},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::get,
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
        // Everything else → embedded web UI
        .fallback(static_handler)
        .layer(middleware::from_fn(validate_local_host))
        .with_state(state)
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
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        Ok(record) if filter.matches_seq(&record) => match serde_json::to_string(&record) {
            Ok(data) => Some(Ok(Event::default().data(data))),
            Err(e) => {
                tracing::error!(err = %e, "failed to serialize log record for SSE");
                None
            }
        },
        Ok(_) => None,
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!(dropped = n, "SSE client lagged");
            None
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── REST ──────────────────────────────────────────────────────────────────────

async fn records_handler(
    State(state): State<Arc<AppState>>,
    AxumQuery(params): AxumQuery<FilterParams>,
) -> impl IntoResponse {
    // Human-facing: no elision, no dedup — the dashboard renders raw lines.
    let q = Query::new(params.into_filter())
        .with_limit(usize::MAX)
        .with_max_payload_chars(usize::MAX);
    Json(state.store.records(&q).records)
}

async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "buffered": state.store.len(),
            "cursor": state.store.cursor(),
        })),
    )
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
        Ok(Query::new(self.build_filter()?)
            .with_limit(self.limit.unwrap_or(default_limit).clamp(1, MAX_LIMIT))
            .with_cursor(self.cursor)
            .with_max_payload_chars(
                self.max_payload_chars
                    .unwrap_or(DEFAULT_MAX_PAYLOAD_CHARS)
                    .clamp(1, MAX_PAYLOAD_CHARS),
            ))
    }
}

/// Raw records, newest-last, with a cursor for incremental follow-up reads.
async fn query_handler(
    State(state): State<Arc<AppState>>,
    AxumQuery(params): AxumQuery<AgentParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let q = params.build_query(100)?;
    Ok(Json(
        serde_json::to_value(state.store.records(&q))
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    ))
}

/// Distinct failures rather than every occurrence — the endpoint an agent
/// should reach for first. Defaults to `level >= error`.
async fn errors_handler(
    State(state): State<Arc<AppState>>,
    AxumQuery(params): AxumQuery<AgentParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut params = params;
    params.level.get_or_insert_with(|| "error".to_string());
    let context_lines = params
        .context_lines
        .unwrap_or(DEFAULT_CONTEXT_LINES)
        .min(MAX_CONTEXT_LINES);
    let q = params.build_query(20)?;
    let summary = state.store.summarize(&q, context_lines);
    Ok(Json(serde_json::to_value(summary).map_err(|e| {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?))
}

async fn sources_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "sources": state.source_views(),
        "buffered": state.store.len(),
        "cursor": state.store.cursor(),
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
    AxumQuery(params): AxumQuery<AgentParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let timeout_ms = params.timeout_ms.unwrap_or(30_000).min(MAX_TIMEOUT_MS);
    let q = params.build_query(100)?;
    let started = Instant::now();

    // Subscribe *before* checking the buffer, so a record landing between the
    // two is caught by the subscription instead of falling through the gap.
    let mut rx = state.tx.subscribe();

    let snapshot = state.store.cursor();
    if let Some(anchor) = state.store.latest_matching_seq(&q) {
        let result = state
            .store
            .records_from(anchor, q.limit, q.max_payload_chars);
        return Ok(Json(wait_result(true, result, 0)));
    }

    let deadline = Duration::from_millis(timeout_ms);
    let matched_seq = tokio::time::timeout(deadline, async {
        loop {
            match rx.recv().await {
                Ok(record)
                    if record.seq > snapshot
                        && q.cursor.is_none_or(|cursor| record.seq > cursor)
                        && q.filter.matches_seq(&record) =>
                {
                    return Some(record.seq)
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "wait subscriber lagged");
                    if let Some(anchor) = state.store.latest_matching_seq(&q) {
                        return Some(anchor);
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await
    .unwrap_or(None);

    let Some(anchor) = matched_seq else {
        let horizon = state.store.records(&q);
        return Ok(Json(serde_json::json!({
            "matched": false,
            "records": [],
            "total_matching": 0,
            "truncated": false,
            "next_cursor": horizon.next_cursor,
            "buffer_start_cursor": horizon.buffer_start_cursor,
            "cursor_gap": horizon.cursor_gap,
            "waited_ms": started.elapsed().as_millis() as u64,
        })));
    };

    // A failure rarely arrives as one line. Let the rest of the burst — the
    // stack trace, the retry, the cascading downstream error — land before
    // answering, so the caller gets the whole event in one round trip.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let result = state
        .store
        .records_from(anchor, q.limit, q.max_payload_chars);
    Ok(Json(wait_result(
        true,
        result,
        started.elapsed().as_millis() as u64,
    )))
}

fn wait_result(
    matched: bool,
    result: tailflow_core::query::QueryResult,
    waited_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "matched": matched,
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

    match WebAssets::get(path) {
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
    }
}

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

    async fn get(state: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
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
}
