//! Command outcomes (M4-E) and evidence-based correlation (M4-F).
//!
//! Both are read-only and bounded, built on indexed history queries.
//!
//! **Outcomes.** A command operation's outcome is only what its source
//! reported:
//! - `reported_success`: a completion hook;
//! - `reported_failure`: a failure hook, with the exit code when the source
//!   gave one;
//! - `interrupted`;
//! - `outcome_unknown`: no outcome reported (Codex without an exit code, a
//!   command still running, a missing hook).
//!
//! `verified_exit_status` is reserved for commands whose exit TailFlow
//! observes itself. It observes none of the agents' commands, so it never
//! appears for them. Nothing is synthesized.
//!
//! **Correlation tiers**, strongest first. Every item says which tier it is
//! in and why:
//! 1. Direct identifiers: the same, parent or child operation ids, as
//!    reported by the source.
//! 2. Independent observation: the same path reported by an agent and seen
//!    by the file watcher within a few seconds.
//! 3. Shared context: the same session or project, close in time.
//! 4. Temporal proximity: runtime errors and warnings within the window,
//!    with no recorded relationship.
//! 5. Inference: a stated heuristic (a runtime error message naming the
//!    changed file) that may be useful but is not confirmed.
//!
//! No item ever claims that one event caused another.

use crate::api_v1::{blocking, parse_time, storage_of, V1Error};
use crate::state::AppState;
use axum::{
    extract::{rejection::QueryRejection, Path as AxPath, Query as AxumQuery, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use chrono::{DateTime, Duration as CD, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use tailflow_core::agent::command::{classify, Category};
use tailflow_storage::{EventQuery, Order, Storage, StoredEvent};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/outcomes", get(outcomes))
        .route("/api/v1/events/{seq}/related", get(related))
}

/// Runtime errors this long after a command's end count as "nearby".
const NEARBY_AFTER: CD = CD::seconds(30);
const MAX_COMMAND_EVENTS: usize = 1_000;
const MAX_RUNTIME_ERRORS: usize = 1_000;

fn s<'a>(e: &'a StoredEvent, k: &str) -> Option<&'a str> {
    e.event.attributes.get(k).and_then(Value::as_str)
}

fn short(text: Option<&str>) -> Option<String> {
    text.map(|t| {
        let mut out: String = t.chars().take(200).collect();
        if t.chars().count() > 200 {
            out.push('…');
        }
        out
    })
}

fn related_project(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
        }
        _ => false,
    }
}

// ── Outcomes ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct OutcomeParams {
    since: Option<String>,
    until: Option<String>,
    session: Option<String>,
    agent: Option<String>,
    project: Option<String>,
    category: Option<String>,
    outcome: Option<String>,
    limit: Option<usize>,
}

const OUTCOMES: &[&str] = &[
    "reported_success",
    "reported_failure",
    "interrupted",
    "outcome_unknown",
    "verified_exit_status",
];

#[derive(Debug, Serialize)]
struct NearbyRef {
    seq: u64,
    at: DateTime<Utc>,
    source_id: String,
    message: Option<String>,
    same_project: bool,
}

#[derive(Debug, Serialize)]
struct CommandOutcome {
    operation_id: Option<String>,
    session_id: Option<String>,
    agent_name: Option<String>,
    project_id: Option<String>,
    program: Option<String>,
    subcommand: Option<String>,
    script: Option<String>,
    category: &'static str,
    /// The rule that decided the category, in words.
    category_rule: String,
    outcome: &'static str,
    /// What the outcome rests on, in words.
    evidence: String,
    exit_code: Option<i64>,
    exit_code_source: Option<String>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    duration_ms: Option<u64>,
    start_seq: Option<u64>,
    end_seq: Option<u64>,
    compound: bool,
    /// Runtime errors from the start to 30 s after the end: close in time,
    /// no recorded relationship.
    nearby_errors: Vec<NearbyRef>,
    nearby_errors_total: u64,
}

#[allow(clippy::too_many_lines)]
fn compute_outcomes(storage: &Storage, p: OutcomeParams) -> Result<Value, V1Error> {
    let now = Utc::now();
    let since = parse_time("since", p.since.as_deref())?.unwrap_or(now - CD::hours(24));
    let until = parse_time("until", p.until.as_deref())?.unwrap_or(now);
    if since >= until {
        return Err(V1Error::bad_request("`since` must be earlier than `until`"));
    }
    let limit = p.limit.unwrap_or(200).clamp(1, 500);
    let want_cat = match p.category.as_deref() {
        None | Some("") => None,
        Some(c) => Some(Category::parse(c).ok_or_else(|| {
            V1Error::bad_request(format!(
                "invalid `category` {c:?} (expected build|test|lint|typecheck|format|deps|other|unknown)"
            ))
        })?),
    };
    if let Some(o) = p
        .outcome
        .as_deref()
        .filter(|o| !o.is_empty() && !OUTCOMES.contains(o))
    {
        return Err(V1Error::bad_request(format!(
            "invalid `outcome` {o:?} (expected {})",
            OUTCOMES.join("|")
        )));
    }
    let one = |v: &Option<String>| {
        v.iter()
            .filter(|x| !x.is_empty())
            .cloned()
            .collect::<Vec<_>>()
    };
    let page = storage.query(&EventQuery {
        event_types: vec!["command.*".into()],
        since: Some(since),
        until: Some(until),
        sessions: one(&p.session),
        agents: one(&p.agent),
        projects: one(&p.project),
        order: Order::Desc,
        limit: MAX_COMMAND_EVENTS,
        ..EventQuery::default()
    })?;
    let truncated = page.has_more;
    // Group start and end events by operation.
    let mut ops: BTreeMap<String, (Option<StoredEvent>, Option<StoredEvent>)> = BTreeMap::new();
    for e in page.events {
        let key = match (&e.event.session_id, &e.event.operation_id) {
            (Some(sid), Some(op)) => format!("{sid}|{op}"),
            _ => format!("seq:{}", e.seq),
        };
        let slot = ops.entry(key).or_default();
        if e.event.event_type == "command.started" {
            slot.0 = Some(e);
        } else if slot.1.is_none() {
            slot.1 = Some(e);
        }
    }
    // Runtime errors over the range (+ the nearby margin), once.
    let errors = storage
        .query(&EventQuery {
            event_types: vec!["runtime.error".into()],
            since: Some(since),
            until: Some(until + NEARBY_AFTER),
            order: Order::Asc,
            limit: MAX_RUNTIME_ERRORS,
            ..EventQuery::default()
        })
        .map(|p| p.events)
        .unwrap_or_default();

    let mut items: Vec<CommandOutcome> = Vec::new();
    for (start, end) in ops.into_values() {
        let any = end.as_ref().or(start.as_ref());
        let Some(any) = any else { continue };
        let ev = &any.event;
        let program = s(any, "command.effective_program").or_else(|| s(any, "command.program"));
        let subcommand = s(any, "command.subcommand");
        let script = s(any, "command.script");
        let (category, rule) = classify(program, subcommand, script);
        let exit_code = end
            .as_ref()
            .and_then(|e| e.event.attributes.get("command.exit_code"))
            .and_then(Value::as_i64);
        let exit_source = end
            .as_ref()
            .and_then(|e| s(e, "command.exit_code_source"))
            .map(str::to_string);
        let (outcome, evidence) = match end
            .as_ref()
            .map(|e| (e.event.event_type.as_str(), e.event.status))
        {
            Some((_, Some(tailflow_core::event::EventStatus::Cancelled))) => (
                "interrupted",
                "the agent reported that the command was interrupted".to_string(),
            ),
            Some(("command.failed", _)) => (
                "reported_failure",
                match exit_code {
                    Some(c) => format!("the agent reported a failure with exit code {c}"),
                    None => "the agent reported a failure (no exit code given)".to_string(),
                },
            ),
            Some(("command.completed", _)) => (
                "reported_success",
                match exit_code {
                    Some(c) => format!("the agent reported completion with exit code {c}"),
                    None => "the agent reported completion (no exit code given)".to_string(),
                },
            ),
            _ => (
                "outcome_unknown",
                "no outcome was reported (still running, or the agent does not report one)"
                    .to_string(),
            ),
        };
        let started_at = start.as_ref().map(|e| e.event.timestamp);
        let ended_at = end.as_ref().map(|e| e.event.timestamp);
        let from = started_at.or(ended_at).unwrap_or(ev.timestamp);
        let to = ended_at.unwrap_or(from) + NEARBY_AFTER;
        let near: Vec<&StoredEvent> = errors
            .iter()
            .filter(|r| r.event.timestamp >= from && r.event.timestamp <= to)
            .collect();
        items.push(CommandOutcome {
            operation_id: ev.operation_id.clone(),
            session_id: ev.session_id.clone(),
            agent_name: ev.agent_name.clone(),
            project_id: ev.project_id.clone(),
            program: program.map(str::to_string),
            subcommand: subcommand.map(str::to_string),
            script: script.map(str::to_string),
            category: category.as_str(),
            category_rule: rule,
            outcome,
            evidence,
            exit_code,
            exit_code_source: exit_source,
            duration_ms: end.as_ref().and_then(|e| e.event.duration_ms).or_else(|| {
                started_at
                    .zip(ended_at)
                    .and_then(|(a, b)| u64::try_from((b - a).num_milliseconds()).ok())
            }),
            started_at,
            ended_at,
            start_seq: start.as_ref().map(|e| e.seq),
            end_seq: end.as_ref().map(|e| e.seq),
            compound: any
                .event
                .attributes
                .get("command.compound")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            nearby_errors_total: near.len() as u64,
            nearby_errors: near
                .iter()
                .take(3)
                .map(|r| NearbyRef {
                    seq: r.seq,
                    at: r.event.timestamp,
                    source_id: r.event.source_id.clone(),
                    message: short(r.event.message.as_deref()),
                    same_project: related_project(
                        r.event.project_id.as_deref(),
                        ev.project_id.as_deref(),
                    ),
                })
                .collect(),
        });
    }
    items.sort_by(|a, b| {
        b.started_at
            .or(b.ended_at)
            .cmp(&a.started_at.or(a.ended_at))
    });
    // Counts before filtering, so a filtered view still shows the totals.
    let mut counts: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    for i in &items {
        *counts
            .entry(i.category)
            .or_default()
            .entry(i.outcome)
            .or_default() += 1;
    }
    items.retain(|i| {
        want_cat.is_none_or(|c| c.as_str() == i.category)
            && p.outcome
                .as_deref()
                .is_none_or(|o| o.is_empty() || o == i.outcome)
    });
    let shown = items.len();
    items.truncate(limit);
    Ok(serde_json::json!({
        "items": items,
        "counts": counts,
        "total": shown,
        "truncated": truncated || shown > limit,
        "nearby_after_ms": NEARBY_AFTER.num_milliseconds(),
        "outcomes": {
            "reported_success": "the agent reported completion; for Claude Code a failing exit fires a separate failure hook",
            "reported_failure": "the agent reported a failure (with its exit code when given)",
            "interrupted": "the agent reported an interruption",
            "outcome_unknown": "no outcome was reported",
            "verified_exit_status": "TailFlow observed the exit itself (never for agent commands)",
        },
    }))
}

async fn outcomes(
    State(state): State<Arc<AppState>>,
    params: Result<AxumQuery<OutcomeParams>, QueryRejection>,
) -> Result<Json<Value>, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let storage = storage_of(&state)?.clone();
    blocking(move || compute_outcomes(&storage, p))
        .await
        .map(Json)
}

// ── Correlation ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RelatedParams {
    window_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
struct Related {
    seq: u64,
    at: DateTime<Utc>,
    event_type: String,
    summary: Option<String>,
    /// What links it to the event, in words.
    explanation: String,
    /// Milliseconds after (+) or before (−) the event.
    delta_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct Tier {
    tier: u8,
    name: &'static str,
    strength: &'static str,
    description: &'static str,
    items: Vec<Related>,
    total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn rel(e: &StoredEvent, anchor: DateTime<Utc>, explanation: String) -> Related {
    Related {
        seq: e.seq,
        at: e.event.timestamp,
        event_type: e.event.event_type.clone(),
        summary: short(e.event.message.as_deref()),
        explanation,
        delta_ms: (e.event.timestamp - anchor).num_milliseconds(),
        session_id: e.event.session_id.clone(),
    }
}

fn ago(ms: i64) -> String {
    let s = ms.unsigned_abs() as f64 / 1000.0;
    if ms >= 0 {
        format!("{s:.1} s after")
    } else {
        format!("{s:.1} s before")
    }
}

const FILE_CHANGES: &[&str] = &[
    "file.created",
    "file.modified",
    "file.deleted",
    "file.renamed",
];

#[allow(clippy::too_many_lines)]
fn compute_related(storage: &Storage, seq: u64, window_ms: i64) -> Result<Option<Value>, V1Error> {
    let Some(e) = storage.get(seq, false)? else {
        return Ok(None);
    };
    let ev = &e.event;
    let at = ev.timestamp;
    let w = CD::milliseconds(window_ms);
    let mut seen: HashSet<u64> = HashSet::from([seq]);
    let mut tiers = Vec::new();

    // Tier 1: direct identifiers.
    let mut t1 = Vec::new();
    let mut t1_total = 0u64;
    if let (Some(sid), true) = (
        &ev.session_id,
        ev.operation_id.is_some() || ev.parent_operation_id.is_some(),
    ) {
        let page = storage.query(&EventQuery {
            sessions: vec![sid.clone()],
            since: Some(at - CD::minutes(30)),
            until: Some(at + CD::minutes(30)),
            order: Order::Asc,
            limit: 1_000,
            ..EventQuery::default()
        })?;
        for x in &page.events {
            if x.seq == seq {
                continue;
            }
            let xe = &x.event;
            let why = if ev.operation_id.is_some() && xe.operation_id == ev.operation_id {
                Some("same operation id (reported by the source)")
            } else if ev.operation_id.is_some() && xe.parent_operation_id == ev.operation_id {
                Some("child of this operation (reported by the source)")
            } else if ev.parent_operation_id.is_some() && xe.operation_id == ev.parent_operation_id
            {
                Some("parent operation (reported by the source)")
            } else if ev.parent_operation_id.is_some()
                && xe.parent_operation_id == ev.parent_operation_id
            {
                Some("same parent operation (reported by the source)")
            } else {
                None
            };
            if let Some(why) = why {
                t1_total += 1;
                seen.insert(x.seq);
                if t1.len() < 20 {
                    t1.push(rel(x, at, why.to_string()));
                }
            }
        }
    }
    tiers.push(Tier {
        tier: 1,
        name: "Direct identifiers",
        strength: "recorded link",
        description: "Linked by operation or session ids the source reported.",
        total: t1_total,
        items: t1,
        note: None,
    });

    // Tier 2: independent observation (file changes only).
    let mut t2 = Vec::new();
    let mut t2_note = None;
    if FILE_CHANGES.contains(&ev.event_type.as_str()) {
        if let Some(path) = &ev.path {
            let window = CD::seconds(5);
            let other = if ev.source_type == tailflow_core::observe::SOURCE_TYPE {
                "agent_hook"
            } else {
                "fs_watcher"
            };
            for r in storage.events_at_path(path, at - window, at + window, other, 20)? {
                if !seen.insert(r.seq) {
                    continue;
                }
                let delta = (r.at - at).num_milliseconds();
                t2.push(Related {
                    seq: r.seq,
                    at: r.at,
                    event_type: r.event_type.clone(),
                    summary: None,
                    explanation: if other == "fs_watcher" {
                        format!("the file watcher saw this path change {} the report (association, not authorship)", ago(delta))
                    } else {
                        format!("an agent reported changing this path {} the observation", ago(delta))
                    },
                    delta_ms: delta,
                    session_id: r.session_id.clone(),
                });
            }
            if t2.is_empty() {
                t2_note = Some(if other == "fs_watcher" {
                    "No filesystem observation of this path within ±5 s (see Files → Verified on disk for whether it was observed at all).".to_string()
                } else {
                    "No agent reported this path within ±5 s.".to_string()
                });
            }
        }
    } else if ev.event_type.starts_with("command.") {
        t2_note = Some("TailFlow does not observe processes: a command's outcome is only what the agent reported.".into());
    }
    let t2_total = t2.len() as u64;
    tiers.push(Tier {
        tier: 2,
        name: "Independent observation",
        strength: "observed",
        description: "The same file change, reported by an agent and seen by the file watcher.",
        total: t2_total,
        items: t2,
        note: t2_note,
    });

    // Tier 3: shared context (same session, then same project), in the window.
    let mut t3 = Vec::new();
    let mut t3_total = 0u64;
    if let Some(sid) = &ev.session_id {
        let page = storage.query(&EventQuery {
            sessions: vec![sid.clone()],
            since: Some(at - w),
            until: Some(at + w),
            order: Order::Asc,
            limit: 200,
            ..EventQuery::default()
        })?;
        for x in &page.events {
            if seen.insert(x.seq) {
                t3_total += 1;
                if t3.len() < 10 {
                    let d = (x.event.timestamp - at).num_milliseconds();
                    t3.push(rel(x, at, format!("same session, {}", ago(d))));
                }
            }
        }
    }
    tiers.push(Tier {
        tier: 3,
        name: "Shared context",
        strength: "context",
        description: "Same session or project, close in time. Context, not a link.",
        total: t3_total,
        items: t3,
        note: None,
    });

    // Tier 4: temporal proximity (runtime errors and warnings, or agent
    // activity around a runtime line).
    let runtime = ev.event_type.starts_with("runtime.");
    let page = storage.query(&EventQuery {
        event_types: if runtime {
            vec!["agent.*".into(), "command.*".into(), "file.*".into()]
        } else {
            vec!["runtime.error".into(), "runtime.warning".into()]
        },
        since: Some(at - w),
        until: Some(at + w),
        order: Order::Asc,
        limit: 200,
        ..EventQuery::default()
    })?;
    let mut t4 = Vec::new();
    let mut t4_total = 0u64;
    let mut later_errors: Vec<StoredEvent> = Vec::new();
    for x in page.events {
        if !seen.insert(x.seq) {
            continue;
        }
        t4_total += 1;
        let d = (x.event.timestamp - at).num_milliseconds();
        let same = related_project(x.event.project_id.as_deref(), ev.project_id.as_deref());
        if t4.len() < 15 {
            t4.push(rel(
                &x,
                at,
                format!(
                    "{}{}; no recorded relationship",
                    ago(d),
                    if same { ", same project" } else { "" }
                ),
            ));
        }
        if d >= 0 && x.event.event_type == "runtime.error" {
            later_errors.push(x);
        }
    }
    tiers.push(Tier {
        tier: 4,
        name: "Temporal proximity",
        strength: "nearby",
        description: "Close in time only. Being nearby does not make events related.",
        total: t4_total,
        items: t4,
        note: page
            .has_more
            .then(|| "More events in the window than listed.".into()),
    });

    // Tier 5: inference. One stated heuristic: a later runtime error whose
    // message names the changed file.
    let mut t5 = Vec::new();
    if FILE_CHANGES.contains(&ev.event_type.as_str()) {
        let base = ev
            .path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| n.len() >= 4);
        if let Some(base) = base {
            for x in &later_errors {
                if x.event
                    .message
                    .as_deref()
                    .is_some_and(|m| m.contains(&base))
                {
                    let d = (x.event.timestamp - at).num_milliseconds();
                    t5.push(rel(
                        x,
                        at,
                        format!(
                            "heuristic: this runtime error {} the change mentions `{base}`; possibly related, not confirmed",
                            ago(d)
                        ),
                    ));
                }
            }
        }
    }
    let t5_total = t5.len() as u64;
    tiers.push(Tier {
        tier: 5,
        name: "Inference",
        strength: "inferred",
        description: "Heuristics that may be useful but are not confirmed.",
        total: t5_total,
        items: t5,
        note: Some(
            "Rule: a runtime error after a file change whose message contains the file's name."
                .into(),
        ),
    });

    Ok(Some(serde_json::json!({
        "seq": seq,
        "event_type": ev.event_type,
        "provenance": ev.provenance,
        "at": at,
        "window_ms": window_ms,
        "tiers": tiers,
        "notice": "Relationships are graded by evidence. None of them is a claim of cause and effect.",
    })))
}

async fn related(
    State(state): State<Arc<AppState>>,
    AxPath(seq): AxPath<u64>,
    params: Result<AxumQuery<RelatedParams>, QueryRejection>,
) -> Result<Json<Value>, V1Error> {
    let AxumQuery(p) = params.map_err(|r| {
        V1Error::bad_request(format!("invalid query parameters: {}", r.body_text()))
    })?;
    let window_ms = p.window_ms.unwrap_or(30_000);
    if !(1_000..=600_000).contains(&window_ms) {
        return Err(V1Error::bad_request("`window_ms` must be 1000..=600000"));
    }
    let storage = storage_of(&state)?.clone();
    match blocking(move || compute_related(&storage, seq, window_ms)).await? {
        Some(v) => Ok(Json(v)),
        None => Err(V1Error {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            message: "no event with that seq".into(),
        }),
    }
}
