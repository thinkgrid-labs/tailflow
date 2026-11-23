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
}

pub struct V1Error {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl V1Error {
    fn bad_request(message: impl Into<String>) -> Self {
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

fn storage_of(state: &AppState) -> Result<&tailflow_storage::Storage, V1Error> {
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

async fn blocking<T: Send + 'static>(
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

async fn list_events(
    State(state): State<Arc<AppState>>,
    V1Query(params): V1Query,
) -> Result<Json<tailflow_storage::EventPage>, V1Error> {
    let q = params.into_query()?;
    let storage = storage_of(&state)?.clone();
    // Serialized straight into the response body: no intermediate `Value`
    // copy of a page that may approach the 8 MiB page budget.
    // No flush-before-read: measured in M1.5, it let any polling client
    // force a commit per poll (3× M1's write amplification) and tripled
    // query latency under ingestion. History trails live ingestion by at
    // most `batch_max_delay_ms`; the live endpoints are not batched.
    let page = blocking(move || storage.query(&q).map_err(V1Error::from)).await?;
    Ok(Json(page))
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

    async fn send(
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
}
