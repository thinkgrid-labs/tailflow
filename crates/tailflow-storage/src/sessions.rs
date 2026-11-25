//! Agent sessions: the `agent_sessions` summary table and its queries.
//!
//! The row for a session is updated in the same transaction that inserts
//! each of its events, and only for events that were actually inserted
//! (never for deduplicated replays), so counts survive restarts, replays
//! and out-of-order delivery. Aggregates are order-independent: earliest
//! and latest are `min`/`max`, counts are sums, and "latest" text fields
//! (end reason, start source) follow the event timestamp, not arrival order.
//!
//! Status is derived at read time from what was *observed*:
//!
//! | status | meaning |
//! |---|---|
//! | `active` | no end observed after the last start, events in the last 30 min |
//! | `ended` | an end event was observed after the last start |
//! | `unterminated` | no end observed and no events for 30 min: crashed, killed, or idle; TailFlow cannot tell which |
//!
//! `lifecycle_observed` says whether start and end were seen at all: a
//! session TailFlow joined mid-way (hooks installed later, events lost)
//! is never presented as complete.

use crate::error::{Result, StorageError};
use crate::query::GapView;
use crate::schema::meta_get_i64;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use tailflow_core::agent::{capabilities, AgentKind, CoverageLevel};
use tailflow_core::event::ObservabilityEvent;

/// A session with no end and no events for this long is `unterminated`.
pub const ACTIVE_WINDOW_SECS: i64 = 30 * 60;
const MAX_LIMIT: usize = 500;

fn attr_str<'a>(e: &'a ObservabilityEvent, k: &str) -> Option<&'a str> {
    e.attributes.get(k).and_then(Value::as_str)
}

/// Maintain the session row for a just-inserted event at `seq`.
pub(crate) fn upsert(c: &Connection, e: &ObservabilityEvent, seq: i64, ts: i64) -> Result<()> {
    let (Some(key), Some(agent)) = (e.session_id.as_deref(), e.agent_name.as_deref()) else {
        return Ok(());
    };
    if e.source_type != tailflow_core::agent::SOURCE_TYPE {
        return Ok(());
    }
    let ty = e.event_type.as_str();
    let is_start = ty == "agent.session.started";
    let is_end = ty == "agent.session.ended";
    let flag = |b: bool| i64::from(b);
    let tool_end = matches!(ty, "agent.tool.completed" | "agent.tool.failed");
    let boundary = is_start || is_end;
    let git_root = boundary.then(|| attr_str(e, "git.root")).flatten();
    let tokens = (ty == "agent.model.completed")
        .then(|| e.attributes.get("tokens.total").and_then(Value::as_i64))
        .flatten();
    let mut stmt = c.prepare_cached(
        "INSERT INTO agent_sessions (
            session_key, agent_name, project_id, working_directory,
            first_seq, last_seq, first_event_at, last_event_at, event_count,
            started_at, last_started_at, start_count, start_source,
            ended_at, end_count, end_reason,
            tool_calls, tool_failures, commands, command_failures, file_events, subagents, turns,
            tokens_total, git_root, git_branch, git_head_start, git_head_end, git_worktree, model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?6, 1,
            CASE WHEN ?7 THEN ?6 END, CASE WHEN ?7 THEN ?6 END, ?7, CASE WHEN ?7 THEN ?8 END,
            CASE WHEN ?9 THEN ?6 END, ?9, CASE WHEN ?9 THEN ?10 END,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, CASE WHEN ?7 THEN ?21 END, CASE WHEN ?9 THEN ?21 END, ?22, ?23)
         ON CONFLICT(session_key) DO UPDATE SET
            project_id = coalesce(CASE WHEN excluded.git_root IS NOT NULL THEN excluded.project_id END,
                                  project_id, excluded.project_id),
            working_directory = coalesce(working_directory, excluded.working_directory),
            first_seq = min(first_seq, excluded.first_seq),
            last_seq = max(last_seq, excluded.last_seq),
            first_event_at = min(first_event_at, excluded.first_event_at),
            last_event_at = max(last_event_at, excluded.last_event_at),
            event_count = event_count + 1,
            started_at = coalesce(min(coalesce(started_at, excluded.started_at),
                                      coalesce(excluded.started_at, started_at)), started_at),
            last_started_at = max(coalesce(last_started_at, excluded.last_started_at),
                                  coalesce(excluded.last_started_at, last_started_at)),
            start_count = start_count + excluded.start_count,
            start_source = CASE WHEN excluded.start_count > 0
                                 AND (last_started_at IS NULL OR excluded.last_started_at >= last_started_at)
                                THEN excluded.start_source ELSE start_source END,
            ended_at = max(coalesce(ended_at, excluded.ended_at), coalesce(excluded.ended_at, ended_at)),
            end_count = end_count + excluded.end_count,
            end_reason = CASE WHEN excluded.end_count > 0
                               AND (ended_at IS NULL OR excluded.ended_at >= ended_at)
                              THEN excluded.end_reason ELSE end_reason END,
            tool_calls = tool_calls + excluded.tool_calls,
            tool_failures = tool_failures + excluded.tool_failures,
            commands = commands + excluded.commands,
            command_failures = command_failures + excluded.command_failures,
            file_events = file_events + excluded.file_events,
            subagents = subagents + excluded.subagents,
            turns = turns + excluded.turns,
            tokens_total = CASE WHEN excluded.tokens_total IS NULL THEN tokens_total
                                ELSE coalesce(tokens_total, 0) + excluded.tokens_total END,
            git_root = coalesce(excluded.git_root, git_root),
            git_branch = coalesce(excluded.git_branch, git_branch),
            git_head_start = coalesce(git_head_start, excluded.git_head_start),
            git_head_end = coalesce(excluded.git_head_end, git_head_end),
            git_worktree = coalesce(excluded.git_worktree, git_worktree),
            model = coalesce(excluded.model, model)",
    )?;
    stmt.execute(params![
        key,
        agent,
        e.project_id,
        // `path` is the working directory for non-file events.
        (!ty.starts_with("file."))
            .then_some(e.path.as_deref())
            .flatten(),
        seq,
        ts,
        flag(is_start),
        attr_str(e, "session.source"),
        flag(is_end),
        attr_str(e, "session.end_reason"),
        flag(tool_end),
        flag(ty == "agent.tool.failed"),
        flag(ty == "command.started"),
        flag(ty == "command.failed"),
        flag(ty.starts_with("file.")),
        flag(ty == "agent.subagent.started"),
        flag(matches!(ty, "agent.turn.completed" | "agent.turn.failed")),
        tokens,
        git_root,
        boundary.then(|| attr_str(e, "git.branch")).flatten(),
        boundary.then(|| attr_str(e, "git.head")).flatten(),
        boundary.then(|| attr_str(e, "git.worktree")).flatten(),
        (is_start || ty == "agent.model.completed")
            .then(|| attr_str(e, "session.model").or_else(|| attr_str(e, "model.name")))
            .flatten(),
    ])?;
    Ok(())
}

/// Remove session rows whose every event is at or below `hi` (expired or
/// purged). Caller holds the transaction.
pub(crate) fn delete_expired(c: &Connection, hi: i64) -> Result<()> {
    c.execute("DELETE FROM agent_sessions WHERE last_seq <= ?1", [hi])?;
    Ok(())
}

/// Filters for [`list`].
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    pub agents: Vec<String>,
    /// Exact project ids (normalized absolute paths).
    pub projects: Vec<String>,
    /// `active`, `ended`, `unterminated`.
    pub statuses: Vec<String>,
    /// Sessions with activity at or after this time.
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitView {
    pub root: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub head_at_start: Option<String>,
    pub head_at_end: Option<String>,
    /// HEAD differed between the observed start and end. Inferred, low
    /// confidence: the developer may have committed in another terminal.
    pub inferred_head_changed: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionCounts {
    pub events: u64,
    pub tool_calls: u64,
    pub tool_failures: u64,
    pub commands: u64,
    pub command_failures: u64,
    pub file_events: u64,
    pub subagents: u64,
    pub turns: u64,
    /// Sum of reported token counts (Codex `PostModelRequest`). `None`
    /// when the agent reported none — not zero.
    pub tokens_total: Option<u64>,
}

/// How much of a session's story TailFlow can vouch for.
#[derive(Debug, Clone, Serialize)]
pub struct SessionCoverage {
    /// `complete` (start and end observed, no recorded loss, nothing
    /// expired), `partial`, or `unknown` (neither boundary observed).
    /// "complete" is always *within the integration's capabilities*.
    pub history: &'static str,
    pub reasons: Vec<&'static str>,
    pub start_observed: bool,
    pub end_observed: bool,
    /// Some of the session's events were removed by retention or purge.
    pub retention_truncated: bool,
    /// Recorded loss overlapping the session (by seq range or time).
    pub gaps: Vec<GapView>,
    /// Activities this agent's integration cannot see, or sees partially.
    pub integration_limits: Vec<IntegrationLimit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntegrationLimit {
    pub activity: &'static str,
    pub level: CoverageLevel,
    pub note: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub agent_name: String,
    pub source_session_id: String,
    pub project_id: Option<String>,
    pub working_directory: Option<String>,
    pub status: &'static str,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<String>,
    pub start_source: Option<String>,
    /// Starts observed after the first (resume, clear, compact).
    pub restarts: u64,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    /// Not exposed by either integration's hook input.
    pub agent_version: Option<String>,
    pub git: GitView,
    pub counts: SessionCounts,
    pub first_seq: u64,
    pub last_seq: u64,
    pub coverage: SessionCoverage,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionView>,
    /// Sessions whose events have all expired are not listed.
    pub evicted_through_seq: u64,
}

const COLUMNS: &str = "session_key, agent_name, project_id, working_directory, first_seq, last_seq,
    first_event_at, last_event_at, event_count, started_at, last_started_at, start_count,
    start_source, ended_at, end_count, end_reason, tool_calls, tool_failures, commands,
    command_failures, file_events, subagents, turns, tokens_total, git_root, git_branch,
    git_head_start, git_head_end, git_worktree, model";

struct Row {
    key: String,
    agent: String,
    project: Option<String>,
    cwd: Option<String>,
    first_seq: i64,
    last_seq: i64,
    first_at: i64,
    last_at: i64,
    events: i64,
    started: Option<i64>,
    last_started: Option<i64>,
    starts: i64,
    start_source: Option<String>,
    ended: Option<i64>,
    ends: i64,
    end_reason: Option<String>,
    counts: [i64; 7],
    tokens: Option<i64>,
    git: [Option<String>; 5],
    model: Option<String>,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        key: r.get(0)?,
        agent: r.get(1)?,
        project: r.get(2)?,
        cwd: r.get(3)?,
        first_seq: r.get(4)?,
        last_seq: r.get(5)?,
        first_at: r.get(6)?,
        last_at: r.get(7)?,
        events: r.get(8)?,
        started: r.get(9)?,
        last_started: r.get(10)?,
        starts: r.get(11)?,
        start_source: r.get(12)?,
        ended: r.get(13)?,
        ends: r.get(14)?,
        end_reason: r.get(15)?,
        counts: [
            r.get(16)?,
            r.get(17)?,
            r.get(18)?,
            r.get(19)?,
            r.get(20)?,
            r.get(21)?,
            r.get(22)?,
        ],
        tokens: r.get(23)?,
        git: [r.get(24)?, r.get(25)?, r.get(26)?, r.get(27)?, r.get(28)?],
        model: r.get(29)?,
    })
}

fn at(us: Option<i64>) -> Option<DateTime<Utc>> {
    us.and_then(DateTime::from_timestamp_micros)
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn status_of(r: &Row, now_us: i64) -> &'static str {
    let ended_after_start = r
        .ended
        .is_some_and(|e| r.last_started.is_none_or(|s| e >= s));
    if ended_after_start {
        "ended"
    } else if now_us.saturating_sub(r.last_at) <= ACTIVE_WINDOW_SECS * 1_000_000 {
        "active"
    } else {
        "unterminated"
    }
}

/// Gaps that overlap a session by seq range, or (for agent losses, which
/// are anchored where they were *recorded* and usually cannot name a
/// session) by time. A session that has not ended is charged with agent
/// loss up to now: an unparseable hook call after its last event may well
/// have been its own.
fn session_gaps(c: &Connection, r: &Row, ended: bool, now_us: i64) -> Result<Vec<GapView>> {
    let until = if ended { r.last_at } else { now_us };
    let mut stmt = c.prepare_cached(
        "SELECT reason, count, first_at, last_at, from_seq, to_seq FROM gaps
         WHERE reason != 'session_deleted' AND (
             (to_seq >= ?1 AND from_seq < ?2)
          OR (reason LIKE 'agent_%' AND first_at <= ?4 AND last_at >= ?3))
         ORDER BY id LIMIT 20",
    )?;
    let rows = stmt.query_map(params![r.first_seq, r.last_seq, r.first_at, until], |g| {
        Ok(GapView {
            reason: g.get(0)?,
            count: g
                .get::<_, Option<i64>>(1)?
                .and_then(|n| u64::try_from(n).ok()),
            first_at: DateTime::from_timestamp_micros(g.get(2)?),
            last_at: DateTime::from_timestamp_micros(g.get(3)?),
            from_seq: u(g.get(4)?),
            to_seq: u(g.get(5)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn view(c: &Connection, r: Row, evicted: i64, now_us: i64) -> Result<SessionView> {
    let status = status_of(&r, now_us);
    let start_observed = r.starts > 0;
    let end_observed = r.ends > 0 && status == "ended";
    let truncated = r.first_seq <= evicted;
    let gaps = session_gaps(c, &r, status == "ended", now_us)?;
    let mut reasons = Vec::new();
    if !start_observed {
        reasons.push("start_not_observed");
    }
    if !end_observed {
        reasons.push(if status == "active" {
            "session_in_progress"
        } else {
            "end_not_observed"
        });
    }
    if truncated {
        reasons.push("retention_truncated");
    }
    if !gaps.is_empty() {
        reasons.push("loss_recorded");
    }
    let history = if reasons.is_empty() {
        "complete"
    } else if !start_observed && !end_observed {
        "unknown"
    } else {
        "partial"
    };
    let kind = AgentKind::parse(&r.agent);
    let limits = kind
        .map(capabilities)
        .unwrap_or_default()
        .iter()
        .filter(|cap| cap.level != CoverageLevel::Direct)
        .map(|cap| IntegrationLimit {
            activity: cap.activity,
            level: cap.level,
            note: cap.note,
        })
        .collect();
    let source_id = r
        .key
        .split_once(':')
        .map_or_else(|| r.key.clone(), |(_, s)| s.to_string());
    let [root, branch, head_start, head_end, worktree] = r.git;
    let head_changed = match (&head_start, &head_end) {
        (Some(a), Some(b)) => Some(a != b),
        _ => None,
    };
    let [tool_calls, tool_failures, commands, command_failures, file_events, subagents, turns] =
        r.counts;
    Ok(SessionView {
        session_id: r.key,
        agent_name: r.agent,
        source_session_id: source_id,
        project_id: r.project,
        working_directory: r.cwd,
        status,
        started_at: at(r.started),
        ended_at: if status == "ended" { at(r.ended) } else { None },
        end_reason: if status == "ended" {
            r.end_reason
        } else {
            None
        },
        start_source: r.start_source,
        restarts: u(r.starts.saturating_sub(1)),
        first_event_at: at(Some(r.first_at)),
        last_event_at: at(Some(r.last_at)),
        model: r.model,
        agent_version: None,
        git: GitView {
            root,
            worktree,
            branch,
            head_at_start: head_start,
            head_at_end: head_end,
            inferred_head_changed: head_changed,
        },
        counts: SessionCounts {
            events: u(r.events),
            tool_calls: u(tool_calls),
            tool_failures: u(tool_failures),
            commands: u(commands),
            command_failures: u(command_failures),
            file_events: u(file_events),
            subagents: u(subagents),
            turns: u(turns),
            tokens_total: r.tokens.map(u),
        },
        first_seq: u(r.first_seq),
        last_seq: u(r.last_seq),
        coverage: SessionCoverage {
            history,
            reasons,
            start_observed,
            end_observed,
            retention_truncated: truncated,
            gaps,
            integration_limits: limits,
        },
    })
}

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidQuery(m.into())
}

/// List sessions, most recent activity first.
///
/// # Errors
/// Invalid filters, or a SQLite failure.
pub fn list(c: &Connection, q: &SessionQuery) -> Result<SessionPage> {
    for (name, vals) in [
        ("agent", &q.agents),
        ("project", &q.projects),
        ("status", &q.statuses),
    ] {
        if vals.len() > 32 {
            return Err(invalid(format!("`{name}` accepts at most 32 values")));
        }
    }
    for s in &q.statuses {
        if !matches!(s.as_str(), "active" | "ended" | "unterminated") {
            return Err(invalid(format!(
                "unknown status {s:?} (expected active, ended or unterminated)"
            )));
        }
    }
    let limit = match q.limit {
        0 => 50,
        n if n > MAX_LIMIT => return Err(invalid(format!("`limit` must be 1..={MAX_LIMIT}"))),
        n => n,
    };
    let evicted = meta_get_i64(c, "evicted_through_seq")?;
    let mut sql = format!("SELECT {COLUMNS} FROM agent_sessions WHERE last_seq > ?1");
    let mut args: Vec<rusqlite::types::Value> = vec![evicted.into()];
    let mut push_in = |sql: &mut String, col: &str, vals: &[String]| {
        if vals.is_empty() {
            return;
        }
        let marks: Vec<String> = vals
            .iter()
            .map(|v| {
                args.push(v.clone().into());
                format!("?{}", args.len())
            })
            .collect();
        sql.push_str(&format!(" AND {col} IN ({})", marks.join(",")));
    };
    let agents: Vec<String> = q
        .agents
        .iter()
        .map(|a| AgentKind::parse(a).map_or_else(|| a.clone(), |k| k.as_str().to_string()))
        .collect();
    push_in(&mut sql, "agent_name", &agents);
    push_in(&mut sql, "project_id", &q.projects);
    if let Some(since) = q.since {
        args.push(since.timestamp_micros().into());
        sql.push_str(&format!(" AND last_event_at >= ?{}", args.len()));
    }
    sql.push_str(" ORDER BY last_event_at DESC");
    // Status is derived, so filter after reading; bound the scan.
    let scan = if q.statuses.is_empty() {
        limit
    } else {
        limit.saturating_mul(20).min(10_000)
    };
    sql.push_str(&format!(" LIMIT {scan}"));
    let now = Utc::now().timestamp_micros();
    let rows: Vec<Row> = {
        let mut stmt = c.prepare(&sql)?;
        let it = stmt.query_map(rusqlite::params_from_iter(args.iter()), row)?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let mut sessions = Vec::new();
    for r in rows {
        if !q.statuses.is_empty() && !q.statuses.iter().any(|s| s == status_of(&r, now)) {
            continue;
        }
        sessions.push(view(c, r, evicted, now)?);
        if sessions.len() >= limit {
            break;
        }
    }
    Ok(SessionPage {
        sessions,
        evicted_through_seq: u(evicted),
    })
}

/// Tool/command/failure summary for one session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: SessionView,
    /// `(tool, calls)`, most used first.
    pub tools: Vec<NameCount>,
    /// `(program, runs)` from `command.program`, most used first.
    pub programs: Vec<NameCount>,
    /// The most recent failed tool calls and commands.
    pub failures: Vec<FailureView>,
    /// Distinct files the agent reported touching (most recent first).
    pub files: Vec<FileView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NameCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureView {
    pub seq: u64,
    pub event_type: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub message: Option<String>,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    pub path: String,
    /// Last reported operation (`file.modified`, `file.read`, …).
    pub last_event_type: String,
    pub events: u64,
}

/// Events scanned for a detail summary; larger sessions summarize their
/// newest events and say so via `coverage.reasons`.
const DETAIL_SCAN: i64 = 50_000;

/// One session with summaries. `None` when unknown (or fully expired).
///
/// # Errors
/// SQLite failure.
pub fn detail(c: &Connection, key: &str) -> Result<Option<SessionDetail>> {
    let evicted = meta_get_i64(c, "evicted_through_seq")?;
    let r = c
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM agent_sessions WHERE session_key = ?1 AND last_seq > ?2"
            ),
            params![key, evicted],
            row,
        )
        .optional()?;
    let Some(r) = r else {
        return Ok(None);
    };
    let now = Utc::now().timestamp_micros();
    let mut session = view(c, r, evicted, now)?;
    // The window of this session's newest events, via the session index.
    let window_from: i64 = c
        .query_row(
            "SELECT seq FROM events WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2",
            params![key, DETAIL_SCAN - 1],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if window_from > 0 {
        session
            .coverage
            .reasons
            .push("summary_sampled_newest_events");
    }
    let names = |sql: &str| -> Result<Vec<NameCount>> {
        let mut stmt = c.prepare(sql)?;
        let rows = stmt.query_map(params![key, window_from], |r| {
            Ok(NameCount {
                name: r.get(0)?,
                count: u(r.get(1)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    let tools = names(
        "SELECT json_extract(attributes, '$.\"tool.name\"') AS t, count(*) FROM events
         WHERE session_id = ?1 AND seq >= ?2 AND event_type IN ('agent.tool.completed','agent.tool.failed')
           AND t IS NOT NULL GROUP BY t ORDER BY count(*) DESC, t LIMIT 20",
    )?;
    let programs = names(
        "SELECT json_extract(attributes, '$.\"command.program\"') AS p, count(*) FROM events
         WHERE session_id = ?1 AND seq >= ?2 AND event_type = 'command.started'
           AND p IS NOT NULL GROUP BY p ORDER BY count(*) DESC, p LIMIT 20",
    )?;
    let failures = {
        let mut stmt = c.prepare(
            "SELECT seq, event_type, ts, message, json_extract(attributes, '$.\"command.exit_code\"')
             FROM events WHERE session_id = ?1 AND seq >= ?2
               AND event_type IN ('agent.tool.failed','command.failed','agent.turn.failed')
             ORDER BY seq DESC LIMIT 20",
        )?;
        let rows = stmt.query_map(params![key, window_from], |r| {
            Ok(FailureView {
                seq: u(r.get(0)?),
                event_type: r.get(1)?,
                timestamp: DateTime::from_timestamp_micros(r.get(2)?),
                message: r.get(3)?,
                exit_code: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let files = {
        let mut stmt = c.prepare(
            "SELECT path, max(seq) AS last, count(*) FROM events
             WHERE session_id = ?1 AND seq >= ?2 AND event_type LIKE 'file.%' AND path IS NOT NULL
             GROUP BY path ORDER BY last DESC LIMIT 50",
        )?;
        let rows: Vec<(String, i64, i64)> = stmt
            .query_map(params![key, window_from], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (path, last, n) in rows {
            let ty: String = c.query_row(
                "SELECT event_type FROM events WHERE seq = ?1",
                [last],
                |r| r.get(0),
            )?;
            out.push(FileView {
                path,
                last_event_type: ty,
                events: u(n),
            });
        }
        out
    };
    Ok(Some(SessionDetail {
        session,
        tools,
        programs,
        failures,
        files,
    }))
}
