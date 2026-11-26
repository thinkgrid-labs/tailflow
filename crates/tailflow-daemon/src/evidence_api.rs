//! Evidence endpoints (M4-C, M4-D): observation status, file evidence and
//! a session's Git before/after comparison. All read-only and bounded.

use crate::api_v1::{blocking, parse_time, storage_of, V1Error};
use crate::state::AppState;
use axum::{
    extract::{rejection::QueryRejection, Path as AxPath, Query as AxumQuery, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tailflow_core::observe::git::{compare, Change, Counts, Snapshot};
use tailflow_storage::evidence::{FileEvidenceQuery, DEFAULT_WINDOW, MAX_ITEMS};
use tailflow_storage::{EventQuery, Order, Storage, StoredEvent};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/observe/status", get(observe_status))
        .route("/api/v1/evidence/files", get(file_evidence))
        .route("/api/v1/sessions/{id}/changes", get(session_changes))
}

async fn observe_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let s = state
        .observe
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Json(serde_json::to_value(s).unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileEvidenceParams {
    since: Option<String>,
    until: Option<String>,
    session: Option<String>,
    agent: Option<String>,
    path_prefix: Option<String>,
    window_ms: Option<u64>,
    limit: Option<usize>,
    /// Keep only these statuses (comma-separated).
    status: Option<String>,
}

const STATUSES: &[&str] = &[
    "matched_observation",
    "reported_only",
    "unverified",
    "filesystem_observed",
    "ambiguous",
];

async fn file_evidence(
    State(state): State<Arc<AppState>>,
    params: Result<AxumQuery<FileEvidenceParams>, QueryRejection>,
) -> Result<Json<Value>, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let storage = storage_of(&state)?.clone();
    let now = Utc::now();
    let since =
        parse_time("since", p.since.as_deref())?.unwrap_or(now - chrono::Duration::hours(24));
    let until = parse_time("until", p.until.as_deref())?.unwrap_or(now);
    let statuses: Vec<String> = p
        .status
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(bad) = statuses.iter().find(|s| !STATUSES.contains(&s.as_str())) {
        return Err(V1Error::bad_request(format!(
            "invalid `status` {bad:?} (expected {})",
            STATUSES.join("|")
        )));
    }
    let q = FileEvidenceQuery {
        since,
        until,
        session: p.session,
        agent: p.agent,
        path_prefix: p.path_prefix,
        window: p.window_ms.map_or(DEFAULT_WINDOW, Duration::from_millis),
        limit: p.limit.unwrap_or(200).min(MAX_ITEMS),
    };
    let mut page = blocking(move || storage.file_evidence(&q).map_err(V1Error::from)).await?;
    if !statuses.is_empty() {
        page.items
            .retain(|i| statuses.iter().any(|s| s == i.status));
    }
    let mut v = serde_json::to_value(page).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "statuses".into(),
            serde_json::json!({
                "matched_observation": "reported by an agent and independently observed on disk within the window; association, not proof of authorship",
                "reported_only": "reported; the path was observed with full coverage and no loss, but no change was seen",
                "unverified": "reported; could not be checked (no coverage, partial coverage, or observer loss)",
                "filesystem_observed": "observed on disk; no agent reported it (unattributed)",
                "ambiguous": "one observed change matches reports from more than one session",
            }),
        );
    }
    Ok(Json(v))
}

// ── GET /api/v1/sessions/{id}/changes ────────────────────────────────────────

#[derive(Debug, Serialize)]
struct SnapshotRef {
    seq: u64,
    at: DateTime<Utc>,
    trigger: Option<String>,
    head: Option<String>,
    branch: Option<String>,
    changes_total: u64,
    /// Every changed path is listed (not truncated, none withheld).
    complete: bool,
}

#[derive(Debug, Serialize)]
struct PathChange {
    path: String,
    /// appeared | changed | cleared (see `git::Delta`); `unchanged`
    /// (pre-existing) paths are listed separately.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<Change>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<Change>,
    /// reported_by_this_session | reported_by_other_sessions |
    /// reported_by_several_sessions | unreported
    evidence: &'static str,
    reported_by_this_session: bool,
    other_sessions: Vec<String>,
    /// Filesystem observations of this path between the snapshots.
    observations: u64,
}

fn snapshot_from_event(e: &StoredEvent) -> Option<(SnapshotRef, Snapshot)> {
    let a = &e.event.attributes;
    let s = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
    let root = s("git.root")?;
    let total = a
        .get("git.changes_total")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let complete = a
        .get("git.changes_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut changes = Vec::new();
    for item in a
        .get("git.changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(path) = item.get("p").and_then(Value::as_str) else {
            continue;
        };
        let status: &'static str = match item.get("s").and_then(Value::as_str).unwrap_or("") {
            "added" => "added",
            "deleted" => "deleted",
            "renamed" => "renamed",
            "copied" => "copied",
            "type_changed" => "type_changed",
            "untracked" => "untracked",
            "conflicted" => "conflicted",
            _ => "modified",
        };
        changes.push(Change {
            path: path.to_string(),
            status,
            staged: item.get("x").and_then(Value::as_bool).unwrap_or(false),
            unstaged: item.get("y").and_then(Value::as_bool).unwrap_or(false),
            from: item.get("from").and_then(Value::as_str).map(str::to_string),
            size: item.get("size").and_then(Value::as_u64),
            mtime_ns: item.get("mtime_ns").and_then(Value::as_i64).map(i128::from),
            lines_added: item.get("add").and_then(Value::as_u64),
            lines_deleted: item.get("del").and_then(Value::as_u64),
        });
    }
    let r = SnapshotRef {
        seq: e.seq,
        at: e.event.timestamp,
        trigger: s("git.trigger"),
        head: s("git.head"),
        branch: s("git.branch"),
        changes_total: total,
        complete,
    };
    let snap = Snapshot {
        worktree_root: root.into(),
        head: r.head.clone(),
        branch: r.branch.clone(),
        detached: a
            .get("git.detached")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        total_changes: usize::try_from(total).unwrap_or(usize::MAX),
        truncated: !complete,
        changes,
        numstat_missing: a.get("git.numstat_missing").is_some(),
        counts: Counts::default(),
        took_ms: 0,
    };
    Some((r, snap))
}

fn snapshots(storage: &Storage, q: EventQuery) -> Result<Vec<StoredEvent>, V1Error> {
    Ok(storage.query(&q)?.events)
}

fn rel(root: &str, path: &str) -> Option<String> {
    path.strip_prefix(root)
        .and_then(|r| r.strip_prefix('/').or_else(|| r.strip_prefix('\\')))
        .map(|r| r.replace('\\', "/"))
}

#[allow(clippy::too_many_lines)]
fn compute_changes(storage: &Storage, id: &str) -> Result<Option<Value>, V1Error> {
    let Some(detail) = storage.session(id)? else {
        return Ok(None);
    };
    let sv = &detail.session;
    let mut limitations: Vec<&str> =
        vec!["Two snapshots show which paths changed between two moments, not who changed them."];
    let Some(root) = sv.git.root.clone() else {
        return Ok(Some(serde_json::json!({
            "session_id": id,
            "available": false,
            "reason": "the session did not run inside a Git worktree",
        })));
    };
    let start = sv.started_at.or(sv.first_event_at).unwrap_or_else(Utc::now);
    let end = sv.ended_at.or(sv.last_event_at);
    let snap_q = |sessions: Vec<String>, until: Option<DateTime<Utc>>, order: Order| EventQuery {
        event_types: vec!["git.snapshot".into()],
        projects: vec![root.clone()],
        sessions,
        until,
        order,
        limit: 20,
        ..EventQuery::default()
    };
    // Boundary snapshots this session triggered.
    let own = snapshots(storage, snap_q(vec![id.to_string()], None, Order::Asc))?;
    let own_start = own.iter().find(|e| {
        e.event
            .attributes
            .get("git.trigger")
            .and_then(Value::as_str)
            == Some("session_start")
    });
    let own_end = own.iter().rev().find(|e| {
        e.event
            .attributes
            .get("git.trigger")
            .and_then(Value::as_str)
            == Some("session_end")
    });
    let baseline_ev = match own_start {
        Some(e) => Some(e.clone()),
        None => {
            limitations.push("No snapshot was taken when the session started; the baseline is the latest earlier snapshot, so changes made in between appear as session changes.");
            snapshots(storage, snap_q(Vec::new(), Some(start), Order::Desc))?
                .into_iter()
                .next()
        }
    };
    let after_ev = match own_end {
        Some(e) => Some(e.clone()),
        None => {
            let latest = snapshots(
                storage,
                snap_q(
                    Vec::new(),
                    end.map(|e| e + chrono::Duration::seconds(5)),
                    Order::Desc,
                ),
            )?;
            if sv.status == "active" {
                limitations.push("The session is still active; compared with the latest snapshot.");
            } else {
                limitations.push("No snapshot was taken when the session ended; compared with the latest snapshot before its last event.");
            }
            latest.into_iter().next()
        }
    };
    let (Some(b_ev), Some(a_ev)) = (baseline_ev, after_ev) else {
        return Ok(Some(serde_json::json!({
            "session_id": id,
            "available": false,
            "repository": root,
            "reason": "no Git snapshots of this repository cover the session (observe the project with [observe] or --observe)",
        })));
    };
    let (Some((b_ref, b_snap)), Some((a_ref, a_snap))) =
        (snapshot_from_event(&b_ev), snapshot_from_event(&a_ev))
    else {
        return Err(V1Error {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal",
            message: "malformed snapshot".into(),
        });
    };
    if b_ref.seq == a_ref.seq {
        limitations.push("Only one snapshot covers the session; nothing can be compared yet.");
    }
    let (deltas, complete) = compare(&b_snap, &a_snap);
    if !complete {
        limitations.push("A snapshot listed only part of the changed paths (cap or privacy boundary); paths beyond it are not compared.");
    }
    // Evidence between the two snapshots: file events (reported and
    // observed) on the compared paths, bounded.
    let file_q = EventQuery {
        event_types: vec!["file.*".into()],
        since: Some(b_ref.at),
        until: Some(a_ref.at + chrono::Duration::seconds(1)),
        order: Order::Asc,
        limit: 1000,
        ..EventQuery::default()
    };
    let page = storage.query(&file_q)?;
    if page.has_more {
        limitations.push("More than 1,000 file events between the snapshots; evidence per path may be incomplete.");
    }
    let mut reported: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut observed: BTreeMap<String, u64> = BTreeMap::new();
    for e in &page.events {
        if e.event.event_type == "file.read" {
            continue;
        }
        let Some(p) = e.event.path.as_deref().and_then(|p| rel(&root, p)) else {
            continue;
        };
        if e.event.source_type == tailflow_core::observe::SOURCE_TYPE {
            *observed.entry(p).or_default() += 1;
        } else if let Some(s) = &e.event.session_id {
            reported.entry(p).or_default().insert(s.clone());
        }
    }
    let mut changes = Vec::new();
    let mut pre_existing = Vec::new();
    for d in deltas {
        if d.kind == "unchanged" {
            pre_existing.push(d.path);
            continue;
        }
        let who = reported.get(&d.path).cloned().unwrap_or_default();
        let mine = who.contains(id);
        let others: Vec<String> = who.into_iter().filter(|s| s != id).collect();
        let evidence = match (mine, others.is_empty()) {
            (true, true) => "reported_by_this_session",
            (true, false) => "reported_by_several_sessions",
            (false, false) => "reported_by_other_sessions",
            (false, true) => "unreported",
        };
        changes.push(PathChange {
            observations: observed.get(&d.path).copied().unwrap_or(0),
            path: d.path,
            kind: d.kind,
            before: d.before,
            after: d.after,
            evidence,
            reported_by_this_session: mine,
            other_sessions: others,
        });
    }
    // Other sessions active in the same repository in the interval.
    let concurrent: Vec<Value> = storage
        .sessions(&tailflow_storage::sessions::SessionQuery {
            since: Some(b_ref.at),
            limit: 50,
            ..tailflow_storage::sessions::SessionQuery::default()
        })
        .map(|p| p.sessions)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.session_id != id && s.git.root.as_deref() == Some(root.as_str()))
        .filter(|s| s.first_event_at.is_none_or(|f| f <= a_ref.at))
        .map(|s| serde_json::json!({ "session_id": s.session_id, "agent": s.agent_name, "status": s.status }))
        .collect();
    if !concurrent.is_empty() {
        limitations.push("Other sessions were active in this repository between the snapshots; changes cannot be credited to one of them by Git state alone.");
    }
    let head_moved = b_ref.head != a_ref.head;
    if head_moved {
        limitations.push("HEAD moved between the snapshots (commit, checkout or reset); committed work no longer shows as a change.");
    }
    Ok(Some(serde_json::json!({
        "session_id": id,
        "available": true,
        "repository": root,
        "baseline": b_ref,
        "after": a_ref,
        "head_moved": head_moved,
        "complete": complete,
        "changes": changes,
        "pre_existing": pre_existing,
        "concurrent_sessions": concurrent,
        "limitations": limitations,
    })))
}

async fn session_changes(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Value>, V1Error> {
    let storage = storage_of(&state)?.clone();
    if id.is_empty() || id.len() > 4096 {
        return Err(V1Error::bad_request("invalid session id"));
    }
    match blocking(move || compute_changes(&storage, &id)).await? {
        Some(v) => Ok(Json(v)),
        None => Err(V1Error {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            message: "no such session".into(),
        }),
    }
}
