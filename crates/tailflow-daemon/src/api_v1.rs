//! Versioned endpoints over the durable event store.
//!
//! Read-only by design: there is no HTTP mutation or ingestion endpoint in
//! this version. Destructive operations are `tailflow-daemon storage …` CLI
//! commands, protected by filesystem permissions instead of a network token.
//!
//! Contract rules shared with the legacy agent endpoints:
//! - Unknown query parameters are a **400**, never silently ignored.
//! - Every response that lists events carries `completeness`.
//! - Errors are `{"error": "...", "kind": "..."}` with no SQL or file paths.

use crate::state::{AppState, StorageSlot};
use axum::{
    extract::{rejection::QueryRejection, FromRequestParts, Path, Query as AxumQuery, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use std::sync::Arc;
use tailflow_core::{
    event::{EventStatus, EVENT_SCHEMA_VERSION, EVENT_TYPES},
    privacy::BUILTIN_RULES,
    query::parse_since,
    LogLevel,
};
use tailflow_storage::{EventQuery, Order, StorageError};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/events", get(list_events))
        .route("/api/v1/events/{seq}", get(get_event))
        .route("/api/v1/storage/status", get(storage_status))
        .route("/api/v1/storage/retention", get(storage_retention))
        .route("/api/v1/collectors/status", get(collectors_status))
        .route("/api/v1/schema", get(schema))
        .route("/api/v1/privacy", get(privacy))
        .route("/api/v1/sessions", get(list_sessions))
        .route("/api/v1/sessions/{id}", get(get_session))
        .route("/api/v1/agents/status", get(agents_status))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/files", get(files))
        .route("/api/v1/stream/agent", get(stream_agent))
        .merge(crate::evidence_api::router())
        .merge(crate::correlation_api::router())
}

// ── GET /api/v1/stream/agent ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StreamParams {
    /// Resume after this seq (exclusive). Absent: start live, or from the
    /// `Last-Event-ID` header an `EventSource` sends when it reconnects.
    after: Option<u64>,
    agent: Option<String>,
    session: Option<String>,
    project: Option<String>,
}

/// Live, resumable stream of committed agent activity (see
/// `agent_stream.rs`).
async fn stream_agent(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    params: Result<AxumQuery<StreamParams>, QueryRejection>,
) -> Result<Response, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let storage = storage_of(&state)?.clone();
    let last_event_id = match headers.get("last-event-id") {
        None => None,
        Some(v) => Some(
            v.to_str()
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .ok_or_else(|| V1Error::bad_request("invalid Last-Event-ID (expected a seq)"))?,
        ),
    };
    // An EventSource reconnects with its original URL (and so a possibly
    // older `after`) plus the newest id it received: resume from whichever
    // is further along. Everything at or below it was already delivered.
    let after = match (p.after, last_event_id) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if after.is_some_and(|a| i64::try_from(a).is_err()) {
        return Err(V1Error::bad_request("`after` is out of range"));
    }
    let filter = crate::agent_stream::Filter {
        agents: list(p.agent.as_ref()),
        sessions: list(p.session.as_ref()),
        projects: list(p.project.as_ref()),
    };
    for (name, l) in [
        ("agent", &filter.agents),
        ("session", &filter.sessions),
        ("project", &filter.projects),
    ] {
        if l.len() > tailflow_storage::query::MAX_LIST_VALUES || l.iter().any(|v| v.len() > 4096) {
            return Err(V1Error::bad_request(format!(
                "`{name}` accepts at most {} values of up to 4096 bytes",
                tailflow_storage::query::MAX_LIST_VALUES
            )));
        }
    }
    match crate::agent_stream::subscribe(&state, storage, filter, after) {
        Some(sse) => Ok(sse.into_response()),
        None => Err(V1Error {
            status: StatusCode::TOO_MANY_REQUESTS,
            kind: "too_many_streams",
            message: format!(
                "at most {} live agent streams per daemon; close another dashboard tab",
                crate::agent_stream::MAX_SUBSCRIBERS
            ),
        }),
    }
}

pub struct V1Error {
    pub(crate) status: StatusCode,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

impl V1Error {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_query",
            message: message.into(),
        }
    }
}

impl IntoResponse for V1Error {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message, "kind": self.kind })),
        )
            .into_response()
    }
}

impl From<StorageError> for V1Error {
    fn from(e: StorageError) -> Self {
        let status = match &e {
            StorageError::InvalidQuery(_) | StorageError::FullTextDisabled => {
                StatusCode::BAD_REQUEST
            }
            StorageError::QueryTimeout => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Detail goes to the log only.
            tracing::error!(err = %e, "storage query failed");
        }
        Self {
            status,
            kind: e.kind(),
            message: e.public_message(),
        }
    }
}

pub(crate) fn storage_of(state: &AppState) -> Result<&tailflow_storage::Storage, V1Error> {
    match &state.storage {
        StorageSlot::Enabled(s) => Ok(s),
        StorageSlot::Disabled => Err(V1Error {
            status: StatusCode::SERVICE_UNAVAILABLE,
            kind: "storage_disabled",
            message: "durable history is disabled ([storage] enabled = false or --no-storage); \
                      the live endpoints (/api/query, /events) still work"
                .into(),
        }),
        StorageSlot::Unavailable { kind, .. } => Err(V1Error {
            status: StatusCode::SERVICE_UNAVAILABLE,
            kind: "storage_unavailable",
            message: format!(
                "durable history is unavailable ({kind}); see GET /api/v1/storage/status"
            ),
        }),
    }
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, V1Error> + Send + 'static,
) -> Result<T, V1Error> {
    tokio::task::spawn_blocking(f).await.map_err(|_| V1Error {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        kind: "internal",
        message: "query task failed".into(),
    })?
}

// ── GET /api/v1/events ───────────────────────────────────────────────────────

/// All list filters accept comma-separated values.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EventParams {
    r#type: Option<String>,
    source: Option<String>,
    session: Option<String>,
    agent: Option<String>,
    project: Option<String>,
    /// Minimum severity.
    severity: Option<String>,
    status: Option<String>,
    /// Glob: `**` crosses `/`, `*` does not.
    path: Option<String>,
    path_prefix: Option<String>,
    command: Option<String>,
    /// Exact program of agent command events (`cargo`, `npm`).
    program: Option<String>,
    /// Full-text terms (ANDed; trailing `*` = prefix).
    q: Option<String>,
    since: Option<String>,
    until: Option<String>,
    after: Option<u64>,
    before: Option<u64>,
    order: Option<String>,
    limit: Option<usize>,
    include_payload: Option<bool>,
}

struct V1Query(EventParams);

impl<S: Send + Sync> FromRequestParts<S> for V1Query {
    type Rejection = V1Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        AxumQuery::<EventParams>::from_request_parts(parts, state)
            .await
            .map(|AxumQuery(p)| V1Query(p))
            .map_err(|r: QueryRejection| {
                V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
            })
    }
}

fn list(v: Option<&String>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

impl EventParams {
    fn into_query(self) -> Result<EventQuery, V1Error> {
        let since = self
            .since
            .as_deref()
            .map(|s| {
                parse_since(s).ok_or_else(|| {
                    V1Error::bad_request(format!(
                        "invalid `since`: {s:?} (expected RFC 3339 or 30s|5m|2h|1d)"
                    ))
                })
            })
            .transpose()?;
        let until = self
            .until
            .as_deref()
            .map(|s| {
                parse_since(s).ok_or_else(|| {
                    V1Error::bad_request(format!(
                        "invalid `until`: {s:?} (expected RFC 3339 or 30s|5m|2h|1d ago)"
                    ))
                })
            })
            .transpose()?;
        let min_severity = self
            .severity
            .as_deref()
            .map(|s| {
                LogLevel::parse(s).ok_or_else(|| {
                    V1Error::bad_request(format!(
                        "invalid `severity`: {s:?} (expected trace|debug|info|warn|error)"
                    ))
                })
            })
            .transpose()?;
        let statuses = list(self.status.as_ref())
            .iter()
            .map(|s| {
                EventStatus::parse(s).ok_or_else(|| {
                    V1Error::bad_request(format!(
                        "invalid `status`: {s:?} (expected started|succeeded|failed|cancelled)"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let order = match self.order.as_deref() {
            None | Some("asc") => Order::Asc,
            Some("desc") => Order::Desc,
            Some(o) => {
                return Err(V1Error::bad_request(format!(
                    "invalid `order`: {o:?} (expected asc|desc)"
                )))
            }
        };
        Ok(EventQuery {
            event_types: list(self.r#type.as_ref()),
            sources: list(self.source.as_ref()),
            sessions: list(self.session.as_ref()),
            agents: list(self.agent.as_ref()),
            projects: list(self.project.as_ref()),
            min_severity,
            statuses,
            path_glob: self.path,
            path_prefix: self.path_prefix,
            command: self.command,
            program: self.program,
            text: self.q,
            since,
            until,
            after_seq: self.after,
            before_seq: self.before,
            order,
            // Bad limits are rejected by the engine (0 or > max), not clamped:
            // a clamp here would be invisible in the response.
            limit: self.limit.unwrap_or(tailflow_storage::query::DEFAULT_LIMIT),
            include_payload: self.include_payload.unwrap_or(false),
        })
    }
}

/// Deprecation notice for `command=` (M4-H): it matches `attributes.command`,
/// which no current collector sets (agent command lines live in `payload`,
/// only in `redacted`/`full` capture). Kept so existing scripts do not break;
/// removed in 0.5. Never widened to search payloads.
const COMMAND_DEPRECATION: &str =
    "`command=` matches attributes.command, which no current collector \
     sets, so it returns no agent commands; use `program=` (agent command events) instead. \
     It will be removed in 0.5";

async fn list_events(
    State(state): State<Arc<AppState>>,
    V1Query(params): V1Query,
) -> Result<Response, V1Error> {
    let deprecated_command = params.command.is_some();
    let q = params.into_query()?;
    let storage = storage_of(&state)?.clone();
    // Serialized straight into the response body: no intermediate `Value`
    // copy of a page that may approach the 8 MiB page budget.
    // No flush-before-read: measured in M1.5, it let any polling client
    // force a commit per poll (3× M1's write amplification) and tripled
    // query latency under ingestion. History trails live ingestion by at
    // most `batch_max_delay_ms`; the live endpoints are not batched.
    let page = blocking(move || storage.query(&q).map_err(V1Error::from)).await?;
    let mut response = Json(page).into_response();
    if deprecated_command {
        let h = response.headers_mut();
        h.insert("deprecation", axum::http::HeaderValue::from_static("true"));
        h.insert(
            "x-tailflow-warning",
            axum::http::HeaderValue::from_static(COMMAND_DEPRECATION),
        );
    }
    Ok(response)
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GetParams {
    include_payload: Option<bool>,
}

async fn get_event(
    State(state): State<Arc<AppState>>,
    Path(seq): Path<String>,
    params: Result<AxumQuery<GetParams>, QueryRejection>,
) -> Result<Json<serde_json::Value>, V1Error> {
    let AxumQuery(params) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let seq: u64 = seq
        .parse()
        .map_err(|_| V1Error::bad_request("event id must be a seq number"))?;
    let storage = storage_of(&state)?.clone();
    let include = params.include_payload.unwrap_or(false);
    let (found, expired) = blocking(move || {
        let found = storage.get(seq, include).map_err(V1Error::from)?;
        let expired = found.is_none() && storage.is_expired(seq).map_err(V1Error::from)?;
        Ok((found, expired))
    })
    .await?;
    match found {
        Some(e) => serde_json::to_value(e).map(Json).map_err(|_| V1Error {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal",
            message: "failed to serialize response".into(),
        }),
        // 410 vs 404: "it existed and retention removed it" is different
        // evidence from "there was never such an event".
        None if expired => Err(V1Error {
            status: StatusCode::GONE,
            kind: "expired",
            message: format!("event {seq} was removed by retention or purge"),
        }),
        None => Err(V1Error {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            message: format!("no event with seq {seq}"),
        }),
    }
}

// ── GET /api/v1/sessions ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SessionParams {
    agent: Option<String>,
    project: Option<String>,
    /// active | ended | unterminated (comma-separated).
    status: Option<String>,
    since: Option<String>,
    /// Sessions that began before this time.
    until: Option<String>,
    /// Case-insensitive substring of id, project, directory or branch.
    q: Option<String>,
    /// `next_cursor` from the previous page.
    cursor: Option<String>,
    limit: Option<usize>,
}

pub(crate) fn parse_time(
    name: &str,
    v: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, V1Error> {
    v.map(|s| {
        parse_since(s).ok_or_else(|| {
            V1Error::bad_request(format!(
                "invalid `{name}`: {s:?} (expected RFC 3339 or 30s|5m|2h|1d)"
            ))
        })
    })
    .transpose()
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    params: Result<AxumQuery<SessionParams>, QueryRejection>,
) -> Result<Json<tailflow_storage::sessions::SessionPage>, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let q = tailflow_storage::sessions::SessionQuery {
        agents: list(p.agent.as_ref()),
        projects: list(p.project.as_ref()),
        statuses: list(p.status.as_ref()),
        since: parse_time("since", p.since.as_deref())?,
        until: parse_time("until", p.until.as_deref())?,
        text: p.q,
        cursor: p.cursor,
        limit: p.limit.unwrap_or(50),
    };
    let storage = storage_of(&state)?.clone();
    let page = blocking(move || storage.sessions(&q).map_err(V1Error::from)).await?;
    Ok(Json(page))
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    params: Result<AxumQuery<NoParams>, QueryRejection>,
) -> Result<Json<tailflow_storage::sessions::SessionDetail>, V1Error> {
    // No parameters are defined; reject any, like every other endpoint.
    params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    if id.is_empty() || id.len() > tailflow_core::event::MAX_ID_LEN {
        return Err(V1Error::bad_request("invalid session id"));
    }
    let storage = storage_of(&state)?.clone();
    let key = id.clone();
    let found = blocking(move || storage.session(&key).map_err(V1Error::from)).await?;
    found.map(Json).ok_or_else(|| V1Error {
        status: StatusCode::NOT_FOUND,
        kind: "not_found",
        message: format!(
            "no session {id:?} (ids look like claude-code:<uuid>; expired sessions are not listed)"
        ),
    })
}

/// Agent integrations as this daemon sees them: ingestion state, the spool,
/// and each integration's capability matrix.
async fn agents_status(
    State(state): State<Arc<AppState>>,
    params: Result<AxumQuery<NoParams>, QueryRejection>,
) -> Result<Json<serde_json::Value>, V1Error> {
    params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let ingest = state
        .agent_ingest
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let spool = tailflow_core::agent::spool::Spool::default_location();
    let stats = tokio::task::spawn_blocking(move || spool.map(|s| s.stats()))
        .await
        .ok()
        .flatten();
    let installation = tokio::task::spawn_blocking(installation_report)
        .await
        .unwrap_or_default();
    let agents: Vec<serde_json::Value> = tailflow_core::agent::AgentKind::ALL
        .iter()
        .map(|k| {
            serde_json::json!({
                "agent": k.as_str(),
                "name": k.display_name(),
                "capabilities": tailflow_core::agent::capabilities(*k),
                "installation": installation.get(k.as_str()),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "ingest": ingest,
        "spool": stats,
        "agents": agents,
    })))
}

/// What `tailflow agent install` recorded, checked against the agent's
/// config file as it is now. Read-only: reads `integrations.json`, the
/// recorded config files and the managed-policy files (all size-bounded),
/// never runs the agent and never writes anything.
///
/// `state`:
/// - `configured` — every recorded hook is present and its binary exists;
/// - `incomplete` — recorded, but hooks are missing, disabled, duplicated,
///   blocked by policy, or point at a binary that no longer exists;
/// - `not_installed` — no install record (hooks added by hand are not
///   detected; `tailflow agent status` checks the default config paths);
/// - `unknown` — the record could not be read.
///
/// None of these says a session is running: activity comes from sessions.
fn installation_report() -> serde_json::Map<String, serde_json::Value> {
    use tailflow_core::agent::{install, AgentKind};
    let mut out = serde_json::Map::new();
    let state =
        tailflow_core::paths::default_agents_dir().map(|d| install::IntegrationState::load(&d));
    for kind in AgentKind::ALL {
        let policy_block = match kind {
            AgentKind::ClaudeCode => install::claude_policy_blocks_hooks()
                .map(|(p, setting)| format!("{} sets {setting}", p.display())),
            AgentKind::Codex => None,
        };
        let warnings: Vec<String> = match kind {
            AgentKind::Codex => install::codex_config_warnings(),
            AgentKind::ClaudeCode => Vec::new(),
        };
        let (status, records) = match &state {
            None => ("unknown", Vec::new()),
            Some(Err(_)) => ("unknown", Vec::new()),
            Some(Ok(st)) => {
                let recs = st.integrations.get(&kind).cloned().unwrap_or_default();
                let mut healthy = !recs.is_empty() && policy_block.is_none() && warnings.is_empty();
                let views: Vec<serde_json::Value> = recs
                    .iter()
                    .map(|r| {
                        let events: Vec<&str> = r.events.iter().map(String::as_str).collect();
                        let i = install::inspect(kind, &r.config_path, &r.binary, &events);
                        let binary_exists = r.binary.exists();
                        if !i.missing.is_empty()
                            || i.hooks_disabled
                            || !i.missing_binaries.is_empty()
                            || !i.duplicated.is_empty()
                            || i.error.is_some()
                            || !binary_exists
                        {
                            healthy = false;
                        }
                        serde_json::json!({
                            "config_path": r.config_path,
                            "installed_at": r.installed_at,
                            "tailflow_version": r.tailflow_version,
                            "binary": r.binary,
                            "binary_exists": binary_exists,
                            "hooks_installed": i.installed,
                            "hooks_missing": i.missing,
                            "hooks_duplicated": i.duplicated,
                            "hooks_disabled": i.hooks_disabled,
                            "missing_binaries": i.missing_binaries,
                            "error": i.error,
                        })
                    })
                    .collect();
                let status = if recs.is_empty() {
                    "not_installed"
                } else if healthy {
                    "configured"
                } else {
                    "incomplete"
                };
                (status, views)
            }
        };
        out.insert(
            kind.as_str().to_string(),
            serde_json::json!({
                "state": status,
                "records": records,
                "policy_block": policy_block,
                "warnings": warnings,
            }),
        );
    }
    out
}

// ── GET /api/v1/stats, /api/v1/files ─────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StatsParams {
    /// Default 24h.
    since: Option<String>,
    until: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    buckets: Option<usize>,
}

fn stats_range(
    since: Option<&str>,
    until: Option<&str>,
    project: Option<&String>,
    agent: Option<&String>,
    buckets: usize,
) -> Result<tailflow_storage::stats::StatsQuery, V1Error> {
    let since = parse_time("since", since)?
        .unwrap_or_else(|| chrono::Utc::now() - chrono::Duration::hours(24));
    Ok(tailflow_storage::stats::StatsQuery {
        since,
        until: parse_time("until", until)?,
        projects: list(project),
        agents: list(agent),
        buckets,
    })
}

/// Overview aggregates: sessions by status, agent activity totals and a
/// time histogram, computed from indexes (never from a page of events).
async fn stats(
    State(state): State<Arc<AppState>>,
    params: Result<AxumQuery<StatsParams>, QueryRejection>,
) -> Result<Json<tailflow_storage::stats::Stats>, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    if p.buckets == Some(0) {
        return Err(V1Error::bad_request("`buckets` must be at least 1"));
    }
    let q = stats_range(
        p.since.as_deref(),
        p.until.as_deref(),
        p.project.as_ref(),
        p.agent.as_ref(),
        p.buckets.unwrap_or(0),
    )?;
    let storage = storage_of(&state)?.clone();
    let out = blocking(move || storage.stats(&q).map_err(V1Error::from)).await?;
    Ok(Json(out))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FilesParams {
    /// Default 7d.
    since: Option<String>,
    until: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    session: Option<String>,
    path_prefix: Option<String>,
    /// Only paths reported by more than one agent.
    multi_agent: Option<bool>,
    limit: Option<usize>,
}

/// Agent-reported file operations grouped by path. Every row is
/// `agent_reported`: nothing here was verified on disk.
async fn files(
    State(state): State<Arc<AppState>>,
    params: Result<AxumQuery<FilesParams>, QueryRejection>,
) -> Result<Json<tailflow_storage::stats::FilesPage>, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    if p.limit == Some(0) {
        return Err(V1Error::bad_request("`limit` must be at least 1"));
    }
    let since = p.since.as_deref().or(Some("7d"));
    let q = tailflow_storage::stats::FilesQuery {
        range: stats_range(
            since,
            p.until.as_deref(),
            p.project.as_ref(),
            p.agent.as_ref(),
            0,
        )?,
        sessions: list(p.session.as_ref()),
        path_prefix: p.path_prefix,
        multi_agent: p.multi_agent.unwrap_or(false),
        limit: p.limit.unwrap_or(0),
    };
    let storage = storage_of(&state)?.clone();
    let out = blocking(move || storage.files(&q).map_err(V1Error::from)).await?;
    Ok(Json(out))
}

// ── Status ───────────────────────────────────────────────────────────────────

/// Storage status for any slot state. Shared with `/health` and the dashboard.
pub async fn storage_status_value(state: &AppState) -> serde_json::Value {
    match &state.storage {
        StorageSlot::Enabled(s) => {
            let s = s.clone();
            let res = tokio::task::spawn_blocking(move || s.status()).await;
            match res {
                Ok(Ok(status)) => serde_json::to_value(status).unwrap_or_default(),
                Ok(Err(e)) => serde_json::json!({
                    "enabled": true,
                    "state": "error",
                    "error": e.public_message(),
                    "kind": e.kind(),
                }),
                Err(_) => serde_json::json!({"enabled": true, "state": "error"}),
            }
        }
        StorageSlot::Disabled => serde_json::json!({ "enabled": false, "state": "disabled" }),
        StorageSlot::Unavailable { kind, message } => serde_json::json!({
            "enabled": true,
            "state": "unavailable",
            "kind": kind,
            // Local operator diagnostics: may include the database path.
            "error": message,
        }),
    }
}

async fn storage_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut v = storage_status_value(&state).await;
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "ingest".into(),
            serde_json::to_value(state.ingest.view()).unwrap_or_default(),
        );
    }
    Json(v)
}

async fn storage_retention(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, V1Error> {
    let storage = storage_of(&state)?.clone();
    let status = blocking(move || storage.status().map_err(V1Error::from)).await?;
    Ok(Json(serde_json::json!({
        "retention": status.retention,
        "oldest_retained_seq": status.db.oldest_seq,
        "oldest_retained_event_at": status.db.oldest_event_at,
        "evicted_through_seq": status.db.evicted_through_seq,
        "used_bytes": status.db.used_bytes,
        "max_bytes": status.retention.max_size_mb.saturating_mul(1024 * 1024),
    })))
}

async fn collectors_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let storage = match state.storage.storage() {
        Some(s) => {
            let s = s.clone();
            let health = s.health();
            let db = tokio::task::spawn_blocking(move || s.status().map(|st| st.db))
                .await
                .ok()
                .and_then(Result::ok);
            serde_json::json!({
                "state": health.writer_state,
                "events_persisted": health.events_persisted,
                "events_dropped": health.events_dropped_total,
                "events_dropped_by_reason": health.events_dropped,
                "loss_detected": health.loss_detected,
                "storage_errors": health.storage_errors,
                "ingestion_queue_depth": health.ingestion_queue_depth,
                "last_commit_at": health.last_commit_at,
                "oldest_retained_event": db.as_ref().and_then(|d| d.oldest_event_at),
                "database_size": db.as_ref().map(|d| d.file_bytes + d.wal_bytes),
            })
        }
        None => serde_json::json!({ "state": match &state.storage {
            StorageSlot::Disabled => "disabled",
            _ => "unavailable",
        }}),
    };
    let ingest = state.ingest.view();
    Json(serde_json::json!({
        "collectors": state.collector_views(),
        "ingest": ingest,
        "storage": storage,
        // Any detected loss, live or durable, in this run.
        "loss_detected": ingest.records_dropped > 0
            || storage.get("loss_detected").and_then(serde_json::Value::as_bool) == Some(true),
    }))
}

async fn schema() -> Json<serde_json::Value> {
    let types: Vec<_> = EVENT_TYPES
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "typical_provenance": t.typical_provenance,
                "collected": t.collected,
                "description": t.description,
            })
        })
        .collect();
    Json(serde_json::json!({
        "schema_version": EVENT_SCHEMA_VERSION,
        "event_types": types,
        "provenance": ["agent_reported", "filesystem_observed", "runtime_observed", "system_observed"],
        "statuses": ["started", "succeeded", "failed", "cancelled"],
    }))
}

async fn privacy(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let p = &state.policy;
    Json(serde_json::json!({
        "capture_enabled": p.enabled,
        "capture_mode": p.mode.as_str(),
        "redaction_active": p.mode.redacts(),
        "redaction_rules": p.redactor().rule_names(),
        "builtin_rules": BUILTIN_RULES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        "applies_to": ["live stream", "legacy endpoints", "MCP", "durable storage"],
        "guarantee": "heuristic: common credential formats are redacted; not every secret can be detected",
    }))
}

#[cfg(test)]
mod tests {
    use crate::routes::router;
    use crate::state::{AppState, Pipeline, StorageSlot};
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use chrono::Utc;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tailflow_core::config::StorageConfig;
    use tailflow_core::privacy::{CaptureMode, CapturePolicy, PrivacyConfig};
    use tailflow_core::processor::Filter;
    use tailflow_core::{LogLevel, LogRecord, LogSender};
    use tailflow_storage::{OpenOptions, Storage};
    use tower::ServiceExt;

    struct Harness {
        state: Arc<AppState>,
        tx: LogSender,
        _dir: tempfile::TempDir,
    }

    fn harness(privacy: PrivacyConfig, with_storage: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let policy = Arc::new(CapturePolicy::from_config(&privacy).unwrap());
        let storage = if with_storage {
            let cfg = StorageConfig {
                path: Some(dir.path().join("events.db")),
                batch_max_delay_ms: 2,
                ..StorageConfig::default()
            };
            StorageSlot::Enabled(
                Storage::open(
                    &cfg,
                    OpenOptions {
                        project_id: Some("/work/app".into()),
                        version: "test".into(),
                        policy: policy.clone(),
                    },
                )
                .unwrap(),
            )
        } else {
            StorageSlot::Disabled
        };
        let (tx, rx) = tailflow_core::new_bus();
        let (state, _fanout) = AppState::with_pipeline(
            rx,
            crate::state::RingLimits {
                records: 10_000,
                bytes: tailflow_core::query::DEFAULT_MAX_BYTES,
            },
            Pipeline {
                filter: Filter::none(),
                policy,
                storage,
                run_key: "testrun".into(),
                shutdown: tokio_util::sync::CancellationToken::new(),
                agent_ingest: crate::agent_ingest::disabled("test"),
                auth: Arc::new(crate::auth::Auth::generate()),
                observe: crate::observe::disabled("test"),
            },
        );
        Harness {
            state,
            tx,
            _dir: dir,
        }
    }

    fn line(source: &str, level: LogLevel, payload: &str) -> LogRecord {
        LogRecord {
            timestamp: Utc::now(),
            source: source.into(),
            level,
            payload: payload.into(),
        }
    }

    impl Harness {
        async fn ingest(&self, records: Vec<LogRecord>) {
            let target = self.state.store.len() + records.len();
            for r in records {
                self.tx.send(r).unwrap();
            }
            for _ in 0..400 {
                if self.state.store.len() >= target {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            self.flush().await;
        }

        async fn flush(&self) {
            if let Some(s) = self.state.storage.storage() {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.flush(Duration::from_secs(10)).unwrap())
                    .await
                    .unwrap();
            }
        }

        async fn get(&self, uri: &str) -> (StatusCode, serde_json::Value) {
            send(self.state.clone(), Request::builder().uri(uri)).await
        }
    }

    /// Send with this run's API token, as a local CLI does.
    async fn send(
        state: Arc<AppState>,
        req: axum::http::request::Builder,
    ) -> (StatusCode, serde_json::Value) {
        let auth = format!("Bearer {}", state.auth.token().expose());
        send_raw(state, req.header(header::AUTHORIZATION, auth)).await
    }

    /// Send exactly the given headers (no credential added).
    async fn send_raw(
        state: Arc<AppState>,
        req: axum::http::request::Builder,
    ) -> (StatusCode, serde_json::Value) {
        let response = router(state)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, json)
    }

    #[tokio::test]
    async fn agent_sessions_are_listed_and_detailed() {
        let h = harness(PrivacyConfig::default(), true);
        let storage = h.state.storage.storage().unwrap().clone();
        let ctx = tailflow_core::agent::HookContext::now();
        for v in [
            serde_json::json!({"hook_event_name":"SessionStart","session_id":"abc-1","cwd":"/work/app","source":"startup"}),
            serde_json::json!({"hook_event_name":"PostToolUseFailure","session_id":"abc-1","cwd":"/work/app",
                "tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"make"},"error":"Exit code 2"}),
            serde_json::json!({"hook_event_name":"SessionEnd","session_id":"abc-1","reason":"clear"}),
        ] {
            let n = tailflow_core::agent::normalize(
                tailflow_core::agent::AgentKind::ClaudeCode,
                &v,
                &ctx,
            )
            .unwrap();
            for e in n.events {
                assert_eq!(storage.submit(e), tailflow_storage::Submitted::Enqueued);
            }
        }
        h.flush().await;
        let (status, body) = h.get("/api/v1/sessions").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let s = &body["sessions"][0];
        assert_eq!(s["session_id"], "claude-code:abc-1");
        assert_eq!(s["status"], "ended");
        assert_eq!(s["counts"]["command_failures"], 1);
        assert_eq!(s["coverage"]["history"], "complete");

        let (status, d) = h.get("/api/v1/sessions/claude-code:abc-1").await;
        assert_eq!(status, StatusCode::OK, "{d}");
        assert_eq!(d["failures"][0]["exit_code"], 2);
        let (status, _) = h.get("/api/v1/sessions/claude-code:nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = h.get("/api/v1/sessions?bogus=1").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "unknown parameters are 400"
        );
        let (status, _) = h.get("/api/v1/sessions?status=finished").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = h.get("/api/v1/sessions/claude-code:abc-1?x=1").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = h.get("/api/v1/sessions?agent=codex").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["sessions"].as_array().unwrap().is_empty());

        let (status, a) = h.get("/api/v1/agents/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(a["agents"].as_array().unwrap().len(), 2);
        assert_eq!(a["ingest"]["enabled"], false);
    }

    #[tokio::test]
    async fn sessions_endpoint_is_explicit_without_storage() {
        let h = harness(PrivacyConfig::default(), false);
        let (status, body) = h.get("/api/v1/sessions").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["kind"], "storage_disabled");
    }

    #[tokio::test]
    async fn agent_endpoints_are_read_only() {
        let h = harness(PrivacyConfig::default(), true);
        for uri in [
            "/api/v1/sessions",
            "/api/v1/agents/status",
            "/api/v1/sessions/x",
        ] {
            let (status, _) =
                send(h.state.clone(), Request::builder().method("POST").uri(uri)).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        }
    }

    #[tokio::test]
    async fn runtime_logs_are_persisted_and_queryable() {
        let h = harness(PrivacyConfig::default(), true);
        h.ingest(vec![
            line(
                "api",
                LogLevel::Error,
                "ERROR connection refused: postgres:5432",
            ),
            line("web", LogLevel::Info, "compiled successfully"),
        ])
        .await;
        let (status, body) = h.get("/api/v1/events").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_type"], "runtime.error");
        assert_eq!(events[0]["source_type"], "runtime");
        assert_eq!(events[0]["provenance"], "runtime_observed");
        assert_eq!(events[0]["project_id"], "/work/app");
        assert_eq!(
            events[0]["attributes"]["legacy_seq"], 1,
            "correlates with /api/query"
        );
        assert!(events[0]["event_id"]
            .as_str()
            .unwrap()
            .starts_with("testrun:"));
        assert_eq!(body["completeness"]["complete"], true);

        let (_, body) = h.get("/api/v1/events?type=runtime.error&q=refused").await;
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
        let (_, body) = h.get("/api/v1/events?order=desc&limit=1").await;
        assert_eq!(body["events"][0]["source_id"], "web");
        assert_eq!(body["has_more"], true);
    }

    #[tokio::test]
    async fn redaction_applies_to_live_legacy_and_durable_paths() {
        let h = harness(PrivacyConfig::default(), true);
        let secret = "s3cr3t-Zx81";
        h.ingest(vec![line(
            "api",
            LogLevel::Info,
            &format!("login password={secret}"),
        )])
        .await;
        for uri in ["/api/records", "/api/query", "/api/v1/events"] {
            let (_, body) = h.get(uri).await;
            let text = body.to_string();
            assert!(!text.contains(secret), "{uri} leaked the secret: {text}");
            assert!(
                text.contains("[REDACTED:secret_assignment]"),
                "{uri}: {text}"
            );
        }
        let (_, health) = h.get("/api/v1/storage/status").await;
        assert_eq!(health["ingest"]["records_redacted"], 1);
    }

    #[tokio::test]
    async fn full_capture_mode_preserves_legacy_verbatim_output() {
        let privacy = PrivacyConfig {
            capture: CaptureMode::Full,
            ..Default::default()
        };
        let h = harness(privacy, false);
        h.ingest(vec![line("api", LogLevel::Info, "password=hunter2")])
            .await;
        let (_, body) = h.get("/api/records").await;
        assert_eq!(body[0]["payload"], "password=hunter2");
    }

    #[tokio::test]
    async fn v1_rejects_unknown_and_malformed_parameters() {
        let h = harness(PrivacyConfig::default(), true);
        for (uri, needle) in [
            ("/api/v1/events?typo=1", "unknown field"),
            ("/api/v1/events?type=file.modifed", "file.modifed"),
            ("/api/v1/events?limit=0", "limit"),
            ("/api/v1/events?limit=5000", "limit"),
            ("/api/v1/events?severity=loud", "severity"),
            ("/api/v1/events?since=yesterday", "since"),
            ("/api/v1/events?order=sideways", "order"),
            ("/api/v1/events?status=maybe", "status"),
            ("/api/v1/events?after=abc", "invalid query parameters"),
        ] {
            let (status, body) = h.get(uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
            assert!(
                body["error"].as_str().unwrap().contains(needle),
                "{uri}: {body}"
            );
            assert!(
                !body["error"].as_str().unwrap().contains("SELECT"),
                "no SQL in errors"
            );
        }
    }

    #[tokio::test]
    async fn event_lookup_distinguishes_missing_from_expired() {
        let h = harness(PrivacyConfig::default(), true);
        h.ingest(vec![line("api", LogLevel::Info, "one")]).await;
        let (_, list) = h.get("/api/v1/events").await;
        let seq = list["events"][0]["seq"].as_u64().unwrap();
        let (status, body) = h.get(&format!("/api/v1/events/{seq}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "one");
        let (status, _) = h.get("/api/v1/events/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = h.get("/api/v1/events/nope").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let path = h.state.storage.storage().unwrap().path().to_path_buf();
        tailflow_storage::admin::purge(&path, tailflow_storage::admin::PurgeTarget::All).unwrap();
        let (status, body) = h.get(&format!("/api/v1/events/{seq}")).await;
        assert_eq!(status, StatusCode::GONE, "{body}");
    }

    #[tokio::test]
    async fn disabled_and_unavailable_storage_are_explicit() {
        let h = harness(PrivacyConfig::default(), false);
        let (status, body) = h.get("/api/v1/events").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["kind"], "storage_disabled");
        let (_, body) = h.get("/api/v1/storage/status").await;
        assert_eq!(body["enabled"], false);
        // Legacy endpoints keep working memory-only.
        h.ingest(vec![line("api", LogLevel::Error, "ERROR x")])
            .await;
        let (status, body) = h.get("/api/errors").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["distinct"], 1);

        let (tx, rx) = tailflow_core::new_bus();
        drop(tx);
        let (state, _) = AppState::with_pipeline(
            rx,
            crate::state::RingLimits {
                records: 10,
                bytes: tailflow_core::query::DEFAULT_MAX_BYTES,
            },
            Pipeline {
                filter: Filter::none(),
                policy: Arc::new(CapturePolicy::from_config(&PrivacyConfig::default()).unwrap()),
                storage: StorageSlot::Unavailable {
                    kind: "not_a_database",
                    message: "x is not a SQLite database".into(),
                },
                run_key: "m".into(),
                shutdown: tokio_util::sync::CancellationToken::new(),
                agent_ingest: crate::agent_ingest::disabled("test"),
                auth: Arc::new(crate::auth::Auth::generate()),
                observe: crate::observe::disabled("test"),
            },
        );
        let (status, body) = send(state.clone(), Request::builder().uri("/api/v1/events")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["kind"], "storage_unavailable");
        let (_, body) = send(state, Request::builder().uri("/health")).await;
        assert_eq!(body["ok"], true, "the daemon still serves");
        assert_eq!(body["storage"]["state"], "unavailable");
    }

    #[tokio::test]
    async fn bus_lag_is_visible_everywhere() {
        let h = harness(PrivacyConfig::default(), true);
        // A current-thread runtime: the fan-out cannot run until we yield, so
        // overfilling the bus deterministically forces `Lagged`.
        for i in 0..(tailflow_core::BUS_CAPACITY + 500) {
            h.tx.send(line("api", LogLevel::Info, &format!("flood {i}")))
                .unwrap();
        }
        for _ in 0..400 {
            if h.state.ingest.view().records_dropped > 0 && h.state.store.len() >= 4096 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        h.flush().await;
        let dropped = h.state.ingest.view().records_dropped;
        assert_eq!(dropped, 500);

        let (_, health) = h.get("/health").await;
        assert_eq!(health["ingest_dropped"], 500);
        assert_eq!(health["storage"]["loss_detected"], true);
        let (_, q) = h.get("/api/query?limit=1000").await;
        assert_eq!(q["ingest_dropped"], 500);
        let (_, q) = h.get("/api/query?source=tailflow").await;
        assert!(q["records"][0]["payload"]
            .as_str()
            .unwrap()
            .contains("500 log records were dropped"));
        let (_, e) = h.get("/api/errors?level=warn").await;
        assert_eq!(e["ingest_dropped"], 500);
        let (_, v1) = h.get("/api/v1/events?limit=1000").await;
        assert_eq!(v1["completeness"]["complete"], false);
        assert_eq!(v1["completeness"]["gaps"][0]["reason"], "ingest_lag");
        let (_, c) = h.get("/api/v1/collectors/status").await;
        assert_eq!(c["loss_detected"], true);
    }

    #[tokio::test]
    async fn collector_lifecycle_is_recorded() {
        let h = harness(PrivacyConfig::default(), true);
        h.state.register_source_kind("api", "process");
        h.state.mark_source_running("api");
        h.state
            .mark_source_exited("api", Some("exit 1 password=hunter2".into()));
        h.ingest(vec![line("api", LogLevel::Info, "hello")]).await;
        let (_, body) = h.get("/api/v1/events?type=system.collector.*").await;
        let types: Vec<_> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["event_type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            types,
            vec!["system.collector.started", "system.collector.error"]
        );
        assert!(
            !body.to_string().contains("hunter2"),
            "collector errors are redacted too"
        );
        let (_, body) = h.get("/api/v1/events?type=runtime.log").await;
        assert_eq!(body["events"][0]["source_type"], "process");
        let (_, c) = h.get("/api/v1/collectors/status").await;
        assert_eq!(c["collectors"][0]["name"], "api");
        assert_eq!(c["collectors"][0]["collector_status"], "failed");
        assert_eq!(c["collectors"][0]["events_received"], 1);
        assert!(c["storage"]["database_size"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn schema_privacy_and_retention_endpoints() {
        let h = harness(PrivacyConfig::default(), true);
        let (_, s) = h.get("/api/v1/schema").await;
        assert_eq!(s["schema_version"], 1);
        assert!(s["event_types"].as_array().unwrap().len() >= 25);
        let (_, p) = h.get("/api/v1/privacy").await;
        assert_eq!(p["capture_mode"], "metadata");
        assert_eq!(p["redaction_active"], true);
        let (status, r) = h.get("/api/v1/storage/retention").await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["retention"]["retention_days"], 30);
        assert_eq!(r["max_bytes"], 2048u64 * 1024 * 1024);
    }

    #[tokio::test]
    async fn browser_origin_guards() {
        let h = harness(PrivacyConfig::default(), false);
        let req = |origin: Option<&str>, site: Option<&str>, mode: Option<&str>| {
            let mut b = Request::builder().uri("/health");
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            if let Some(s) = site {
                b = b.header("sec-fetch-site", s);
            }
            if let Some(m) = mode {
                b = b.header("sec-fetch-mode", m);
            }
            b
        };
        for (origin, site, mode, expected) in [
            (None, None, None, StatusCode::OK),
            (
                Some("http://localhost:7878"),
                Some("same-origin"),
                Some("cors"),
                StatusCode::OK,
            ),
            (Some("http://127.0.0.1:5173"), None, None, StatusCode::OK),
            (Some("http://[::1]:7878"), None, None, StatusCode::OK),
            (
                Some("https://evil.example"),
                Some("cross-site"),
                Some("cors"),
                StatusCode::FORBIDDEN,
            ),
            (
                Some("http://localhost.evil.example"),
                None,
                None,
                StatusCode::FORBIDDEN,
            ),
            (
                Some("http://127.0.0.1.nip.io:7878"),
                None,
                None,
                StatusCode::FORBIDDEN,
            ),
            (Some("null"), None, None, StatusCode::FORBIDDEN),
            (
                None,
                Some("cross-site"),
                Some("no-cors"),
                StatusCode::FORBIDDEN,
            ),
            (None, Some("cross-site"), Some("navigate"), StatusCode::OK),
        ] {
            let (status, _) = send(h.state.clone(), req(origin, site, mode)).await;
            assert_eq!(
                status, expected,
                "origin={origin:?} site={site:?} mode={mode:?}"
            );
        }
        let long = format!("/api/query?grep={}", "a".repeat(9000));
        let (status, _) = h.get(&long).await;
        assert_eq!(status, StatusCode::URI_TOO_LONG);
    }

    #[tokio::test]
    async fn sse_stream_still_delivers_records() {
        let h = harness(PrivacyConfig::default(), true);
        let response = router(h.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", h.state.auth.token().expose()),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        h.tx.send(line("api", LogLevel::Info, "token=abcdef123456 live"))
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("SSE frame")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(
            text.starts_with("data: "),
            "default `message` events: {text}"
        );
        assert!(text.contains("\"seq\":1"));
        assert!(!text.contains("abcdef123456"), "SSE is redacted: {text}");
    }

    fn agent_events(h: &Harness, session: &str, payloads: &[serde_json::Value]) {
        let storage = h.state.storage.storage().unwrap().clone();
        let ctx = tailflow_core::agent::HookContext::now();
        for v in payloads {
            let mut v = v.clone();
            v["session_id"] = serde_json::json!(session);
            let n = tailflow_core::agent::normalize(
                tailflow_core::agent::AgentKind::ClaudeCode,
                &v,
                &ctx,
            )
            .unwrap();
            for e in n.events {
                assert_eq!(storage.submit(e), tailflow_storage::Submitted::Enqueued);
            }
        }
    }

    #[tokio::test]
    async fn stats_and_files_aggregate_from_storage() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(
            &h,
            "s-1",
            &[
                serde_json::json!({"hook_event_name":"SessionStart","cwd":"/work/app","source":"startup"}),
                serde_json::json!({"hook_event_name":"PostToolUse","cwd":"/work/app","tool_name":"Read",
                    "tool_use_id":"r1","tool_input":{"file_path":"/work/app/src/main.rs"},"tool_response":{}}),
                serde_json::json!({"hook_event_name":"PostToolUseFailure","cwd":"/work/app","tool_name":"Bash",
                    "tool_use_id":"b1","tool_input":{"command":"cargo test"},"error":"Exit code 101"}),
            ],
        );
        h.ingest(vec![line("api", LogLevel::Error, "boom")]).await;

        let (status, st) = h.get("/api/v1/stats?since=1h&buckets=6").await;
        assert_eq!(status, StatusCode::OK, "{st}");
        assert_eq!(st["activity"]["tool_calls"], 2);
        assert_eq!(st["activity"]["command_failures"], 1);
        assert_eq!(st["activity"]["file_reads"], 1);
        assert_eq!(st["sessions"]["active"], 1);
        assert_eq!(st["runtime_errors"], 1);
        assert_eq!(st["buckets"].as_array().unwrap().len(), 6);
        assert_eq!(
            st["agents"].as_array().unwrap().len(),
            2,
            "both known agents listed"
        );

        let (status, cmds) = h.get("/api/v1/events?type=command.*&program=cargo").await;
        assert_eq!(status, StatusCode::OK, "{cmds}");
        let cmds = cmds["events"].as_array().unwrap();
        assert!(!cmds.is_empty());
        assert!(cmds
            .iter()
            .all(|e| e["attributes"]["command.program"] == "cargo"));
        let (_, none) = h.get("/api/v1/events?type=command.*&program=npm").await;
        assert!(none["events"].as_array().unwrap().is_empty());

        let (status, f) = h.get("/api/v1/files?agent=claude").await;
        assert_eq!(status, StatusCode::OK, "{f}");
        // Agent paths are stored in the platform's native form (`\work\…`
        // on Windows), so compare with what the adapter itself produces.
        let expected = tailflow_core::agent::normalize_path("/work/app/src/main.rs", None)
            .unwrap()
            .display()
            .to_string();
        assert_eq!(f["files"][0]["path"], expected.as_str());
        assert_eq!(f["files"][0]["provenance"], "agent_reported");

        for bad in [
            "/api/v1/stats?bogus=1",
            "/api/v1/stats?buckets=0",
            "/api/v1/stats?buckets=100000",
            "/api/v1/stats?since=yesterday",
            "/api/v1/stats?since=1h&until=2h",
            "/api/v1/files?limit=0",
            "/api/v1/files?multi_agent=maybe",
            "/api/v1/files?nope=1",
        ] {
            let (status, body) = h.get(bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
            assert!(body["error"].is_string(), "{bad}");
        }
        for uri in ["/api/v1/stats", "/api/v1/files"] {
            let (status, _) =
                send(h.state.clone(), Request::builder().method("POST").uri(uri)).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri} is read-only");
        }
    }

    #[tokio::test]
    async fn aggregates_are_explicit_without_storage() {
        let h = harness(PrivacyConfig::default(), false);
        for uri in ["/api/v1/stats", "/api/v1/files"] {
            let (status, body) = h.get(uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["kind"], "storage_disabled");
        }
    }

    #[tokio::test]
    async fn session_list_pages_with_a_cursor_over_http() {
        let h = harness(PrivacyConfig::default(), true);
        for i in 0..3 {
            agent_events(
                &h,
                &format!("p-{i}"),
                &[
                    serde_json::json!({"hook_event_name":"SessionStart","cwd":"/work/app","source":"startup"}),
                ],
            );
            // Distinct last-activity times.
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        h.flush().await;
        let (status, p1) = h.get("/api/v1/sessions?limit=2").await;
        assert_eq!(status, StatusCode::OK, "{p1}");
        assert_eq!(p1["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(p1["has_more"], true);
        let cursor = p1["next_cursor"].as_str().unwrap().to_string();
        let encoded: String = cursor
            .bytes()
            .map(|b| match b {
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect();
        let (status, p2) = h
            .get(&format!("/api/v1/sessions?limit=2&cursor={encoded}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{p2}");
        assert_eq!(p2["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(p2["has_more"], false);
        let (status, q) = h.get("/api/v1/sessions?q=P-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(q["sessions"][0]["session_id"], "claude-code:p-1");
        let (status, _) = h.get("/api/v1/sessions?cursor=garbage").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = h.get("/api/v1/sessions?until=soon").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn agent_status_reports_installation_without_claiming_activity() {
        let h = harness(PrivacyConfig::default(), true);
        let (status, a) = h.get("/api/v1/agents/status").await;
        assert_eq!(status, StatusCode::OK, "{a}");
        for agent in a["agents"].as_array().unwrap() {
            let state = agent["installation"]["state"].as_str().unwrap();
            assert!(
                ["configured", "incomplete", "not_installed", "unknown"].contains(&state),
                "{agent}"
            );
            assert!(agent["installation"]["records"].is_array());
        }
    }

    #[tokio::test]
    async fn ui_is_served_with_a_strict_content_security_policy() {
        let h = harness(PrivacyConfig::default(), false);
        for uri in ["/", "/some/client/route"] {
            let response = router(h.state.clone())
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(
                            header::AUTHORIZATION,
                            format!("Bearer {}", h.state.auth.token().expose()),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let headers = response.headers();
            let csp = headers
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(csp.contains("script-src 'self'"), "{uri}: {csp}");
            assert!(csp.contains("frame-ancestors 'none'"), "{uri}");
            assert!(!csp.contains("unsafe-inline"), "{uri}");
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        }
    }

    #[tokio::test]
    async fn ring_catch_up_resumes_after_a_seq_and_names_the_process() {
        let h = harness(PrivacyConfig::default(), false);
        h.ingest(vec![
            line("api", LogLevel::Info, "one"),
            line("api", LogLevel::Info, "two"),
            line("api", LogLevel::Info, "three"),
        ])
        .await;
        let (_, all) = h.get("/api/records").await;
        let all = all.as_array().unwrap().clone();
        assert_eq!(all.len(), 3);
        let first = all[0]["seq"].as_u64().unwrap();
        let (status, rest) = h.get(&format!("/api/records?after={first}")).await;
        assert_eq!(status, StatusCode::OK);
        let rest = rest.as_array().unwrap();
        assert_eq!(rest.len(), 2);
        assert!(rest.iter().all(|r| r["seq"].as_u64().unwrap() > first));
        let (_, health) = h.get("/health").await;
        assert!(health["instance_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
    }

    // ── live agent stream (M4-B) ──────────────────────────────────────────────

    /// One parsed SSE frame.
    #[derive(Debug, Clone)]
    struct Frame {
        event: String,
        id: Option<String>,
        data: String,
    }

    /// An open `/api/v1/stream/agent` response, read frame by frame.
    struct StreamReader {
        body: axum::body::Body,
        buf: String,
    }

    impl StreamReader {
        async fn open(
            h: &Harness,
            uri: &str,
            extra: &[(&str, &str)],
        ) -> (StatusCode, Option<Self>) {
            let mut b = Request::builder().uri(uri).header(
                header::AUTHORIZATION,
                format!("Bearer {}", h.state.auth.token().expose()),
            );
            for (k, v) in extra {
                b = b.header(*k, *v);
            }
            let response = router(h.state.clone())
                .oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            if status != StatusCode::OK {
                return (status, None);
            }
            (
                status,
                Some(Self {
                    body: response.into_body(),
                    buf: String::new(),
                }),
            )
        }

        /// Next non-comment frame, or `None` at end of stream / timeout.
        async fn next(&mut self, timeout: Duration) -> Option<Frame> {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if let Some(end) = self.buf.find("\n\n") {
                    let raw: String = self.buf.drain(..end + 2).collect();
                    let mut f = Frame {
                        event: "message".into(),
                        id: None,
                        data: String::new(),
                    };
                    let mut any = false;
                    for line in raw.lines() {
                        if let Some(v) = line.strip_prefix("event:") {
                            f.event = v.trim().into();
                            any = true;
                        } else if let Some(v) = line.strip_prefix("id:") {
                            f.id = Some(v.trim().into());
                        } else if let Some(v) = line.strip_prefix("data:") {
                            f.data.push_str(v.strip_prefix(' ').unwrap_or(v));
                            any = true;
                        }
                    }
                    if any {
                        return Some(f);
                    }
                    continue; // keep-alive comment
                }
                let frame = tokio::time::timeout_at(deadline, self.body.frame())
                    .await
                    .ok()??;
                let bytes = frame.ok()?.into_data().ok()?;
                self.buf.push_str(&String::from_utf8_lossy(&bytes));
            }
        }

        /// Frames until (and including) the first `event`, bounded.
        async fn until(&mut self, event: &str, timeout: Duration) -> Vec<Frame> {
            let mut out = Vec::new();
            while let Some(f) = self.next(timeout).await {
                let done = f.event == event;
                out.push(f);
                if done {
                    break;
                }
            }
            out
        }

        /// `agent` frames, collected until `n` arrived or `timeout`.
        async fn agents(&mut self, n: usize, timeout: Duration) -> Vec<Frame> {
            let mut out = Vec::new();
            let deadline = tokio::time::Instant::now() + timeout;
            while out.len() < n {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match self.next(left).await {
                    Some(f) if f.event == "agent" => out.push(f),
                    Some(_) => {}
                    None => break,
                }
            }
            out
        }
    }

    fn seqs(frames: &[Frame]) -> Vec<u64> {
        frames
            .iter()
            .filter(|f| f.event == "agent")
            .map(|f| f.id.as_deref().unwrap().parse().unwrap())
            .collect()
    }

    fn tool_calls(n: usize, prefix: &str) -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| {
                serde_json::json!({"hook_event_name":"PreToolUse","cwd":"/work/app","tool_name":"Read",
                    "tool_use_id": format!("{prefix}-{i}"),"tool_input":{"file_path":"/work/app/a.rs"}})
            })
            .collect()
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn agent_stream_delivers_committed_events_live_in_seq_order() {
        let h = harness(PrivacyConfig::default(), true);
        let (status, s) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let mut s = s.unwrap();
        let ready = s.until("ready", WAIT).await;
        assert_eq!(ready.last().unwrap().event, "ready", "{ready:?}");
        // Runtime logs are not agent activity.
        h.ingest(vec![line("api", LogLevel::Error, "boom")]).await;
        agent_events(&h, "s-live", &tool_calls(5, "t"));
        h.flush().await;
        let got = s.agents(5, WAIT).await;
        let seq = seqs(&got);
        assert_eq!(seq.len(), 5, "{got:?}");
        assert!(seq.windows(2).all(|w| w[0] < w[1]), "{seq:?}");
        for f in &got {
            let v: serde_json::Value = serde_json::from_str(&f.data).unwrap();
            assert_eq!(v["seq"].to_string(), *f.id.as_ref().unwrap());
            assert_eq!(v["provenance"], "agent_reported");
            assert!(v["event_type"].as_str().unwrap().starts_with("agent."));
        }
    }

    #[tokio::test]
    async fn agent_stream_resumes_from_a_cursor_without_duplicates() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(&h, "s-hist", &tool_calls(6, "h"));
        h.ingest(vec![line("api", LogLevel::Info, "noise")]).await;
        // Everything so far, from the start.
        let (_, s) = StreamReader::open(&h, "/api/v1/stream/agent?after=0", &[]).await;
        let mut s = s.unwrap();
        let all = s.until("ready", WAIT).await;
        let all_seqs = seqs(&all);
        assert_eq!(all_seqs.len(), 6, "{all:?}");
        let ready: serde_json::Value = serde_json::from_str(&all.last().unwrap().data).unwrap();
        assert_eq!(ready["caught_up_events"], 6);
        // Resume after the third one: only the rest, once.
        let cut = all_seqs[2];
        let (_, s2) =
            StreamReader::open(&h, &format!("/api/v1/stream/agent?after={cut}"), &[]).await;
        let resumed = seqs(&s2.unwrap().until("ready", WAIT).await);
        assert_eq!(resumed, all_seqs[3..].to_vec());
        // The header an EventSource sends on reconnect works the same.
        let (_, s3) = StreamReader::open(
            &h,
            "/api/v1/stream/agent",
            &[("last-event-id", &cut.to_string())],
        )
        .await;
        assert_eq!(
            seqs(&s3.unwrap().until("ready", WAIT).await),
            all_seqs[3..].to_vec()
        );
        // A native EventSource reconnect: stale `after` in the URL, newer
        // Last-Event-ID. Resume from the newer one.
        let (_, s5) = StreamReader::open(
            &h,
            &format!("/api/v1/stream/agent?after={}", all_seqs[0]),
            &[("last-event-id", &cut.to_string())],
        )
        .await;
        assert_eq!(
            seqs(&s5.unwrap().until("ready", WAIT).await),
            all_seqs[3..].to_vec()
        );
        // New events after a reconnect continue from the cursor.
        let mut s4 = StreamReader::open(
            &h,
            &format!("/api/v1/stream/agent?after={}", all_seqs[5]),
            &[],
        )
        .await
        .1
        .unwrap();
        s4.until("ready", WAIT).await;
        agent_events(&h, "s-hist", &tool_calls(2, "later"));
        h.flush().await;
        let more = seqs(&s4.agents(2, WAIT).await);
        assert_eq!(more.len(), 2);
        assert!(more[0] > all_seqs[5]);
    }

    #[tokio::test]
    async fn agent_stream_filters_by_session_and_rejects_bad_parameters() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(&h, "s-a", &tool_calls(3, "a"));
        agent_events(&h, "s-b", &tool_calls(4, "b"));
        h.flush().await;
        // Stored session ids carry the agent (`<agent>:<id>`).
        let (_, page) = h.get("/api/v1/events?type=agent.*&limit=100").await;
        let sid = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["session_id"].as_str().unwrap().to_string())
            .find(|s| s.ends_with("s-b"))
            .unwrap();
        let (_, s) = StreamReader::open(
            &h,
            &format!("/api/v1/stream/agent?after=0&session={sid}"),
            &[],
        )
        .await;
        let frames = s.unwrap().until("ready", WAIT).await;
        let got: Vec<serde_json::Value> = frames
            .iter()
            .filter(|f| f.event == "agent")
            .map(|f| serde_json::from_str(&f.data).unwrap())
            .collect();
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|v| v["session_id"] == sid.as_str()));
        for bad in [
            "/api/v1/stream/agent?bogus=1",
            "/api/v1/stream/agent?after=x",
            "/api/v1/stream/agent?after=99999999999999999999",
        ] {
            let (status, _) = StreamReader::open(&h, bad, &[]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let (status, _) =
            StreamReader::open(&h, "/api/v1/stream/agent", &[("last-event-id", "abc")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn agent_stream_reports_loss_and_expired_history_as_gaps() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(&h, "s-g", &tool_calls(3, "g"));
        let storage = h.state.storage.storage().unwrap().clone();
        let (_, s) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
        let mut s = s.unwrap();
        s.until("ready", WAIT).await;
        // Loss detected by this run: the next commit records a gap and a
        // system.events.dropped marker.
        let now = Utc::now();
        storage.record_upstream_loss(7, now, now);
        agent_events(&h, "s-g", &tool_calls(1, "after-loss"));
        h.flush().await;
        let mut saw_gap = false;
        let mut saw_marker = false;
        while let Some(f) = s.next(Duration::from_secs(3)).await {
            if f.event == "gap" {
                let g: serde_json::Value = serde_json::from_str(&f.data).unwrap();
                assert_eq!(g["count"], 7, "{g}");
                saw_gap = true;
            }
            if f.event == "agent" && f.data.contains("system.events.dropped") {
                saw_marker = true;
            }
            if saw_gap && saw_marker {
                break;
            }
        }
        assert!(saw_gap && saw_marker, "loss is reported, not skipped");

        // History removed while a client was away: a retention gap first.
        let path = storage.path().to_path_buf();
        let report = tokio::task::spawn_blocking(move || {
            tailflow_storage::admin::purge(&path, tailflow_storage::admin::PurgeTarget::All)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(report.deleted > 0);
        // A cursor this database never issued (from a replaced database):
        // reset to the newest seq, not silence.
        let (_, s3) = StreamReader::open(&h, "/api/v1/stream/agent?after=999999999", &[]).await;
        let frames = s3.unwrap().until("ready", WAIT).await;
        assert!(
            frames
                .iter()
                .any(|f| f.event == "reset" && f.data.contains("cursor_ahead")),
            "{frames:?}"
        );
        let (_, s2) = StreamReader::open(&h, "/api/v1/stream/agent?after=1", &[]).await;
        let frames = s2.unwrap().until("ready", WAIT).await;
        assert!(
            frames
                .iter()
                .any(|f| f.event == "gap" && f.data.contains("retention")),
            "{frames:?}"
        );
    }

    #[tokio::test]
    async fn agent_stream_recovers_from_lag_through_storage() {
        let h = harness(PrivacyConfig::default(), true);
        let (_, s) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
        let mut s = s.unwrap();
        s.until("ready", WAIT).await;
        // Do not read while far more batches than the channel holds are
        // published (one commit and one feed read per event).
        let total = 90;
        for i in 0..total {
            agent_events(&h, "s-lag", &tool_calls(1, &format!("lag{i}")));
            h.flush().await;
            tokio::time::sleep(Duration::from_millis(70)).await;
        }
        let mut seqs_seen = Vec::new();
        let mut lagged = false;
        while seqs_seen.len() < total {
            let Some(f) = s.next(WAIT).await else { break };
            match f.event.as_str() {
                "agent" => seqs_seen.push(f.id.unwrap().parse::<u64>().unwrap()),
                "lag" => lagged = true,
                _ => {}
            }
        }
        assert!(lagged, "the subscriber lagged and said so");
        assert_eq!(seqs_seen.len(), total, "nothing lost");
        assert!(
            seqs_seen.windows(2).all(|w| w[0] < w[1]),
            "no duplicates, in order"
        );
        let stats = h.state.agent_feed.as_ref().unwrap().stats();
        assert!(stats.subscriber_lag_recoveries >= 1, "{stats:?}");
    }

    #[tokio::test]
    async fn agent_stream_is_bounded_and_ends_on_shutdown() {
        let h = harness(PrivacyConfig::default(), true);
        let mut open = Vec::new();
        for _ in 0..crate::agent_stream::MAX_SUBSCRIBERS {
            let (status, s) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
            assert_eq!(status, StatusCode::OK);
            open.push(s.unwrap());
        }
        let (status, _) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // A closed client frees its slot.
        drop(open.pop());
        let (status, s) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let mut s = s.unwrap();
        s.until("ready", WAIT).await;
        h.state.shutdown.cancel();
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while s.next(Duration::from_secs(5)).await.is_some() {}
        })
        .await;
        assert!(ended.is_ok(), "the stream ends on shutdown");
    }

    #[tokio::test]
    async fn agent_stream_needs_storage_and_a_credential() {
        let h = harness(PrivacyConfig::default(), false);
        let (status, _) = StreamReader::open(&h, "/api/v1/stream/agent", &[]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let h = harness(PrivacyConfig::default(), true);
        let (status, _) = send_raw(
            h.state.clone(),
            Request::builder().uri("/api/v1/stream/agent"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_command_filter_is_deprecated_not_widened() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(&h, "s-dep", &[bash("d1", "cargo test", "pre")]);
        h.flush().await;
        let auth = format!("Bearer {}", h.state.auth.token().expose());
        let response = router(h.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events?command=cargo")
                    .header(header::AUTHORIZATION, auth.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["deprecation"], "true");
        assert!(response.headers()["x-tailflow-warning"]
            .to_str()
            .unwrap()
            .contains("program="));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["events"].as_array().unwrap().is_empty(),
            "still never matches payloads"
        );
        let (_, p) = h.get("/api/v1/events?program=cargo").await;
        assert!(
            !p["events"].as_array().unwrap().is_empty(),
            "program= is the supported filter"
        );
        let response = router(h.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events?program=cargo")
                    .header(header::AUTHORIZATION, auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.headers().get("deprecation").is_none());
    }

    // ── command outcomes and correlation (M4-E, M4-F) ─────────────────────────

    fn bash(id: &str, cmd: &str, phase: &str) -> serde_json::Value {
        let mut v = serde_json::json!({"cwd": "/work/app", "tool_name": "Bash", "tool_use_id": id,
            "tool_input": {"command": cmd}});
        match phase {
            "pre" => v["hook_event_name"] = "PreToolUse".into(),
            "ok" => {
                v["hook_event_name"] = "PostToolUse".into();
                v["tool_response"] =
                    serde_json::json!({"stdout": "", "stderr": "", "interrupted": false});
            }
            "interrupted" => {
                v["hook_event_name"] = "PostToolUse".into();
                v["tool_response"] =
                    serde_json::json!({"stdout": "", "stderr": "", "interrupted": true});
            }
            _ => {
                v["hook_event_name"] = "PostToolUseFailure".into();
                v["error"] = "Exit code 1\nerror: test failed".into();
            }
        }
        v
    }

    #[tokio::test]
    async fn outcomes_report_only_what_the_agent_reported() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(
            &h,
            "s-out",
            &[
                bash("t1", "cd web && npm test", "pre"),
                bash("t1", "cd web && npm test", "fail"),
                bash("b1", "cargo build --release", "pre"),
                bash("b1", "cargo build --release", "ok"),
                bash("l1", "cargo clippy", "pre"),
                bash("l1", "cargo clippy", "interrupted"),
                bash("r1", "cargo test", "pre"),
            ],
        );
        h.ingest(vec![line("api", LogLevel::Error, "boom after the test")])
            .await;
        let (status, v) = h.get("/api/v1/outcomes?since=1h").await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let items = v["items"].as_array().unwrap();
        let by = |sub: &str, prog: &str| {
            items
                .iter()
                .find(|i| i["subcommand"] == sub && i["program"] == prog)
                .unwrap_or_else(|| panic!("{sub} {prog}: {v}"))
                .clone()
        };
        let t = by("test", "npm");
        assert_eq!(t["category"], "test");
        assert_eq!(t["outcome"], "reported_failure");
        assert_eq!(t["exit_code"], 1);
        assert!(t["nearby_errors_total"].as_u64().unwrap() >= 1, "{t}");
        assert!(t["evidence"].as_str().unwrap().contains("reported"));
        let b = by("build", "cargo");
        assert_eq!(
            (b["category"].as_str(), b["outcome"].as_str()),
            (Some("build"), Some("reported_success"))
        );
        let l = by("clippy", "cargo");
        assert_eq!(
            (l["category"].as_str(), l["outcome"].as_str()),
            (Some("lint"), Some("interrupted"))
        );
        let r = by("test", "cargo");
        assert_eq!(
            r["outcome"], "outcome_unknown",
            "no end hook: no synthetic success"
        );
        // Never claimed for agent commands.
        assert!(items.iter().all(|i| i["outcome"] != "verified_exit_status"));
        assert!(v["counts"]["test"]["reported_failure"].as_u64() >= Some(1));

        let (_, f) = h
            .get("/api/v1/outcomes?since=1h&category=test&outcome=reported_failure")
            .await;
        assert_eq!(f["items"].as_array().unwrap().len(), 1, "{f}");
        for bad in [
            "/api/v1/outcomes?category=nope",
            "/api/v1/outcomes?outcome=succeeded",
            "/api/v1/outcomes?since=1h&until=2h",
            "/api/v1/outcomes?bogus=1",
        ] {
            let (status, _) = h.get(bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn related_events_are_graded_by_evidence_never_causation() {
        let h = harness(PrivacyConfig::default(), true);
        agent_events(
            &h,
            "s-rel",
            &[
                bash("t1", "cargo test", "pre"),
                bash("t1", "cargo test", "fail"),
                serde_json::json!({"hook_event_name":"PostToolUse","cwd":"/work/app","tool_name":"Edit",
                    "tool_use_id":"e1","tool_input":{"file_path":"/work/app/src/handler.rs"},"tool_response":{}}),
            ],
        );
        // An observation of the edited path and a runtime error naming it.
        let storage = h.state.storage.storage().unwrap().clone();
        let path = tailflow_core::agent::normalize_path("/work/app/src/handler.rs", None)
            .unwrap()
            .display()
            .to_string();
        let mut obs = tailflow_core::event::ObservabilityEvent::new(
            "fs-1",
            "file.modified",
            tailflow_core::event::Provenance::FilesystemObserved,
            tailflow_core::observe::SOURCE_TYPE,
            "fs:/work/app",
            Utc::now(),
        );
        obs.path = Some(path.clone());
        let _ = storage.submit(obs);
        h.ingest(vec![line(
            "api",
            LogLevel::Error,
            "panic in handler.rs line 42",
        )])
        .await;
        let (_, page) = h
            .get("/api/v1/events?type=command.failed,file.modified&limit=10")
            .await;
        let events = page["events"].as_array().unwrap();
        let cmd = events
            .iter()
            .find(|e| e["event_type"] == "command.failed")
            .unwrap();
        let edit = events
            .iter()
            .find(|e| e["event_type"] == "file.modified" && e["provenance"] == "agent_reported")
            .unwrap();

        let (status, r) = h
            .get(&format!("/api/v1/events/{}/related", cmd["seq"]))
            .await;
        assert_eq!(status, StatusCode::OK, "{r}");
        let tier = |r: &serde_json::Value, n: u64| {
            r["tiers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["tier"] == n)
                .unwrap()
                .clone()
        };
        let t1 = tier(&r, 1);
        assert!(
            t1["total"].as_u64().unwrap() >= 2,
            "tool and command events share ids: {t1}"
        );
        assert!(tier(&r, 2)["note"]
            .as_str()
            .unwrap()
            .contains("does not observe processes"));
        let t4 = tier(&r, 4);
        assert!(
            t4["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["summary"].as_str().is_some_and(|s| s.contains("panic"))),
            "{t4}"
        );
        let text = r.to_string().to_lowercase();
        for banned in ["caused", "because of", "due to", "led to"] {
            assert!(
                !text.contains(banned),
                "no causal wording ({banned}): {text}"
            );
        }

        let (_, r) = h
            .get(&format!("/api/v1/events/{}/related", edit["seq"]))
            .await;
        let t2 = tier(&r, 2);
        assert_eq!(
            t2["items"].as_array().unwrap().len(),
            1,
            "the observation: {t2}"
        );
        assert!(t2["items"][0]["explanation"]
            .as_str()
            .unwrap()
            .contains("not authorship"));
        let t5 = tier(&r, 5);
        assert!(
            t5["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["explanation"].as_str().unwrap().contains("not confirmed")),
            "heuristic names the file: {t5}"
        );
        // Each item once, in its strongest tier: the heuristic match is not
        // also listed as merely nearby.
        let t5_seqs: Vec<u64> = t5["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["seq"].as_u64())
            .collect();
        assert!(
            tier(&r, 4)["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|i| !t5_seqs.contains(&i["seq"].as_u64().unwrap())),
            "{r}"
        );

        let (status, _) = h.get("/api/v1/events/999999/related").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = h
            .get(&format!(
                "/api/v1/events/{}/related?window_ms=10",
                cmd["seq"]
            ))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
