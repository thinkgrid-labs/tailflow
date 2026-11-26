//! Read-only aggregates for the dashboard: overview counts and per-file
//! activity.
//!
//! Every count here is exact for what is *retained*, and each one comes
//! from an index rather than from a page of events:
//!
//! - **Ingested volume** is a seq span. `seq` is assigned in ingestion order
//!   and `observed_at` never decreases with it, so the events ingested in
//!   `[t0, t1)` are exactly the seqs between two `events_observed` index
//!   seeks. One bucket costs two `O(log n)` probes, whatever the volume.
//! - **Runtime errors and warnings** are counted per bucket with
//!   `event_type = ? AND seq BETWEEN …`, a covering range of `events_type`
//!   (the index carries the rowid): no table lookups, even in a crash loop.
//! - **Agent, command and file events** are counted per exact type with
//!   their event timestamp, agent and project, so the cost is proportional
//!   to the agent events in range, not to the runtime log volume.
//!
//! Statements run under the same wall-clock guard as `/api/v1/events`.

use crate::error::{Result, StorageError};
use crate::query::ProgressGuard;
use crate::retention::first_seq_at_or_after;
use crate::schema::meta_get_i64;
use crate::sessions::derive_status;
use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Instant;
use tailflow_core::agent::AgentKind;
use tailflow_core::event::EVENT_TYPES;

/// Buckets in the overview histogram by default, and at most.
pub const DEFAULT_BUCKETS: usize = 48;
pub const MAX_BUCKETS: usize = 240;
/// Session rows read for one overview; beyond this the counts say `capped`.
pub const MAX_SESSIONS_SCANNED: usize = 10_000;
/// Values in one list filter.
const MAX_LIST_VALUES: usize = 32;
/// Rows returned by [`files`] by default, and at most.
pub const DEFAULT_FILES_LIMIT: usize = 200;
pub const MAX_FILES_LIMIT: usize = 1_000;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidQuery(m.into())
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Filters shared by both aggregates.
#[derive(Debug, Clone)]
pub struct StatsQuery {
    pub since: DateTime<Utc>,
    /// Defaults to now.
    pub until: Option<DateTime<Utc>>,
    pub projects: Vec<String>,
    pub agents: Vec<String>,
    /// Histogram buckets; 0 = default.
    pub buckets: usize,
}

impl StatsQuery {
    fn resolve(&self) -> Result<(i64, i64, Vec<String>)> {
        let since = self.since.timestamp_micros();
        let until = self
            .until
            .unwrap_or_else(Utc::now)
            .timestamp_micros()
            .max(since.saturating_add(1));
        if self.until.is_some_and(|u| u <= self.since) {
            return Err(invalid("`since` must be earlier than `until`"));
        }
        for (name, list) in [("project", &self.projects), ("agent", &self.agents)] {
            if list.len() > MAX_LIST_VALUES {
                return Err(invalid(format!(
                    "`{name}` accepts at most {MAX_LIST_VALUES} values"
                )));
            }
            if list.iter().any(|v| v.is_empty() || v.len() > 4096) {
                return Err(invalid(format!("`{name}` values must be 1..=4096 bytes")));
            }
        }
        // Accept aliases (`claude`) the way the sessions listing does.
        let agents = self
            .agents
            .iter()
            .map(|a| AgentKind::parse(a).map_or_else(|| a.clone(), |k| k.as_str().to_string()))
            .collect();
        Ok((since, until, agents))
    }
}

/// Append `AND col IN (…)` with bound values.
fn push_in(sql: &mut String, args: &mut Vec<SqlValue>, col: &str, vals: &[String]) {
    if vals.is_empty() {
        return;
    }
    let marks: Vec<String> = vals
        .iter()
        .map(|v| {
            args.push(SqlValue::Text(v.clone()));
            format!("?{}", args.len())
        })
        .collect();
    sql.push_str(&format!(" AND {col} IN ({})", marks.join(", ")));
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionTotals {
    /// Sessions with any activity in the range.
    pub total: u64,
    pub active: u64,
    pub ended: u64,
    pub unterminated: u64,
    /// More than [`MAX_SESSIONS_SCANNED`] sessions overlapped the range; the
    /// numbers above cover the most recent ones only.
    pub capped: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActivityTotals {
    /// Finished tool calls (`agent.tool.completed` + `agent.tool.failed`).
    pub tool_calls: u64,
    pub tool_failures: u64,
    /// `command.started`.
    pub commands: u64,
    /// `command.failed` (non-zero exit or interrupted, as reported).
    pub command_failures: u64,
    pub turn_failures: u64,
    pub subagents: u64,
    /// All agent-reported `file.*` events.
    pub file_events: u64,
    pub file_reads: u64,
    /// `file.created` + `file.modified` + `file.deleted` + `file.renamed`.
    pub file_changes: u64,
    /// Distinct failed operations: tool failures + turn failures + command
    /// failures whose tool call did not itself fail. A failed Claude Code
    /// Bash call (reported as both a tool and a command failure) counts once.
    pub failures: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub agent: String,
    pub sessions: u64,
    pub active_sessions: u64,
    #[serde(flatten)]
    pub activity: ActivityTotals,
    /// Newest session of this agent, in or out of the range.
    pub latest_session_id: Option<String>,
    pub latest_session_status: Option<&'static str>,
    pub latest_activity_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Bucket {
    pub start: Option<DateTime<Utc>>,
    /// Events ingested (all sources) — by ingestion time.
    pub ingested: u64,
    pub runtime_errors: u64,
    pub runtime_warnings: u64,
    /// Agent-reported events — by event time.
    pub agent_events: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub sessions: SessionTotals,
    pub activity: ActivityTotals,
    pub agents: Vec<AgentSummary>,
    /// Events ingested in the range, all sources: the seq span, so it
    /// includes events later deleted on request (`tailflow-daemon storage
    /// delete-session`) but not those already removed by retention.
    /// Unaffected by the project and agent filters.
    pub events_ingested: u64,
    /// Runtime log lines detected as error / warn, all sources.
    pub runtime_errors: u64,
    pub runtime_warnings: u64,
    pub bucket_secs: u64,
    pub buckets: Vec<Bucket>,
    /// The range starts before the oldest retained event: older activity
    /// existed and is not counted.
    pub retention_truncated: bool,
    pub history_starts_at: Option<DateTime<Utc>>,
    /// Recorded loss overlapping the range.
    pub gaps_in_range: u64,
}

const AGENT_PREFIXES: [&str; 3] = ["agent.", "command.", "file."];

fn tally(t: &mut ActivityTotals, ty: &str, n: u64) {
    let add = |f: &mut u64| *f = f.saturating_add(n);
    match ty {
        "agent.tool.completed" => add(&mut t.tool_calls),
        "agent.tool.failed" => {
            add(&mut t.tool_calls);
            add(&mut t.tool_failures);
        }
        "command.started" => add(&mut t.commands),
        // `failures` (distinct failed operations) is counted separately.
        "command.failed" => add(&mut t.command_failures),
        "agent.turn.failed" => add(&mut t.turn_failures),
        "agent.subagent.started" => add(&mut t.subagents),
        "file.read" => {
            add(&mut t.file_events);
            add(&mut t.file_reads);
        }
        _ if ty.starts_with("file.") => {
            add(&mut t.file_events);
            add(&mut t.file_changes);
        }
        _ => {}
    }
}

/// The operation a failure event belongs to. A failed tool call is its own
/// operation; a failed command belongs to its parent tool call, so a Claude
/// Code Bash failure (reported as both) yields one key, and a Codex command
/// failure (no failed tool call) yields its own. Without ids, each event is
/// its own operation. Scoped by session.
pub(crate) const FAILURE_KEY: &str = "coalesce(session_id, '') || '|' || coalesce(
    CASE event_type WHEN 'command.failed' THEN parent_operation_id ELSE operation_id END,
    'seq:' || seq)";
/// Event types that mark a failed operation.
pub(crate) const FAILURE_TYPES: &str =
    "('agent.tool.failed', 'command.failed', 'agent.turn.failed')";

/// Bucket width in µs for `span_us` split `n` ways: whole seconds, at
/// least one. The last bucket ends at `until`.
fn bucket_width_us(span_us: i64, n: usize) -> i64 {
    let span = u64::try_from(span_us).unwrap_or(0);
    let n = u64::try_from(n.max(1)).unwrap_or(1);
    let secs = span.div_ceil(n).div_ceil(1_000_000).max(1);
    i64::try_from(secs.saturating_mul(1_000_000)).unwrap_or(i64::MAX)
}

/// Overview aggregates. Blocking.
///
/// # Errors
/// Invalid filters, the time budget, or a SQLite failure.
pub(crate) fn overview(conn: &mut Connection, q: &StatsQuery) -> Result<Stats> {
    let (since, until, agents) = q.resolve()?;
    let n_buckets = match q.buckets {
        0 => DEFAULT_BUCKETS,
        n if n > MAX_BUCKETS => {
            return Err(invalid(format!("`buckets` must be 1..={MAX_BUCKETS}")))
        }
        n => n,
    };
    let guarded = ProgressGuard::install(conn, Instant::now() + crate::query::QUERY_TIME_BUDGET)?;
    let c: &Connection = &guarded;
    let n_i64 = i64::try_from(n_buckets).unwrap_or(1);
    let width = bucket_width_us(until.saturating_sub(since), n_buckets);

    // Seq boundaries of every bucket, by ingestion time.
    let mut bounds = Vec::with_capacity(n_buckets + 1);
    for i in 0..=n_i64 {
        let t = since.saturating_add(width.saturating_mul(i)).min(until);
        bounds.push(first_seq_at_or_after(c, t)?.unwrap_or(1));
    }
    let lo = bounds.first().copied().unwrap_or(1);
    let hi = bounds.last().copied().unwrap_or(1);
    let mut buckets: Vec<Bucket> = (0..n_i64)
        .map(|i| Bucket {
            start: DateTime::from_timestamp_micros(since.saturating_add(width.saturating_mul(i))),
            ..Bucket::default()
        })
        .collect();
    for (b, w) in buckets.iter_mut().zip(bounds.windows(2)) {
        if let [a, z] = w {
            b.ingested = to_u64(z.saturating_sub(*a));
        }
    }

    // Runtime errors / warnings per bucket: covering index ranges.
    let mut runtime = [0u64; 2];
    {
        let mut stmt = c.prepare_cached(
            "SELECT count(*) FROM events WHERE event_type = ?1 AND seq >= ?2 AND seq < ?3",
        )?;
        for (k, ty) in ["runtime.error", "runtime.warning"].iter().enumerate() {
            for (b, w) in buckets.iter_mut().zip(bounds.windows(2)) {
                let [a, z] = w else { continue };
                if a >= z {
                    continue;
                }
                let n: i64 = stmt.query_row(rusqlite::params![ty, a, z], |r| r.get(0))?;
                let n = to_u64(n);
                if k == 0 {
                    b.runtime_errors = n;
                } else {
                    b.runtime_warnings = n;
                }
                if let Some(slot) = runtime.get_mut(k) {
                    *slot = slot.saturating_add(n);
                }
            }
        }
    }

    // Agent activity per exact type, by event time: index-only ranges of
    // `events_agent_activity` (event_type, ts, agent_name, project_id).
    let mut totals = ActivityTotals::default();
    let mut per_agent: BTreeMap<String, ActivityTotals> = BTreeMap::new();
    let types: Vec<&str> = EVENT_TYPES
        .iter()
        .map(|t| t.name)
        .filter(|n| AGENT_PREFIXES.iter().any(|p| n.starts_with(p)))
        .collect();
    for ty in types {
        let mut args: Vec<SqlValue> = vec![
            SqlValue::Text(ty.to_string()),
            SqlValue::Integer(since),
            SqlValue::Integer(until),
            SqlValue::Integer(width),
        ];
        let mut sql = String::from(
            "SELECT agent_name, (ts - ?2) / ?4, count(*) FROM events INDEXED BY events_agent_activity
             WHERE event_type = ?1 AND agent_name IS NOT NULL AND ts >= ?2 AND ts < ?3",
        );
        push_in(&mut sql, &mut args, "project_id", &q.projects);
        push_in(&mut sql, &mut args, "agent_name", &agents);
        sql.push_str(" GROUP BY 1, 2");
        let mut stmt = c.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (agent, bucket, n) = row?;
            let n = to_u64(n);
            tally(&mut totals, ty, n);
            if !agent.is_empty() {
                tally(per_agent.entry(agent).or_default(), ty, n);
            }
            if let Some(b) = usize::try_from(bucket)
                .ok()
                .and_then(|i| buckets.get_mut(i))
            {
                b.agent_events = b.agent_events.saturating_add(n);
            }
        }
    }

    // Distinct failed operations: one pass over the failure events in range
    // (three `events_agent_activity` ranges; a table lookup only per failure,
    // for its ids), grouped by operation key; each operation lands in its
    // earliest bucket.
    {
        let mut args: Vec<SqlValue> = vec![
            SqlValue::Integer(since),
            SqlValue::Integer(until),
            SqlValue::Integer(width),
        ];
        let mut inner = format!(
            "SELECT agent_name AS agent, (ts - ?1) / ?3 AS bucket, {FAILURE_KEY} AS k
             FROM events INDEXED BY events_agent_activity
             WHERE event_type IN {FAILURE_TYPES} AND agent_name IS NOT NULL
               AND ts >= ?1 AND ts < ?2"
        );
        push_in(&mut inner, &mut args, "project_id", &q.projects);
        push_in(&mut inner, &mut args, "agent_name", &agents);
        let sql = format!("SELECT min(agent), min(bucket) FROM ({inner}) GROUP BY k");
        let mut stmt = c.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (agent, bucket) = row?;
            totals.failures = totals.failures.saturating_add(1);
            if !agent.is_empty() {
                let a = per_agent.entry(agent).or_default();
                a.failures = a.failures.saturating_add(1);
            }
            if let Some(b) = usize::try_from(bucket)
                .ok()
                .and_then(|i| buckets.get_mut(i))
            {
                b.failures = b.failures.saturating_add(1);
            }
        }
    }

    // Sessions overlapping the range, most recent first.
    let mut sessions = SessionTotals::default();
    let mut per_agent_sessions: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let now = Utc::now().timestamp_micros();
    {
        let mut args: Vec<SqlValue> = vec![
            SqlValue::Integer(meta_get_i64(c, "evicted_through_seq")?),
            SqlValue::Integer(since),
            SqlValue::Integer(until),
        ];
        let mut sql = String::from(
            "SELECT agent_name, last_started_at, ended_at, last_event_at FROM agent_sessions
             WHERE last_seq > ?1 AND last_event_at >= ?2 AND first_event_at < ?3",
        );
        push_in(&mut sql, &mut args, "project_id", &q.projects);
        push_in(&mut sql, &mut args, "agent_name", &agents);
        sql.push_str(&format!(
            " ORDER BY last_event_at DESC LIMIT {}",
            MAX_SESSIONS_SCANNED.saturating_add(1)
        ));
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for (i, row) in rows.enumerate() {
            if i >= MAX_SESSIONS_SCANNED {
                sessions.capped = true;
                break;
            }
            let (agent, started, ended, last) = row?;
            let status = derive_status(started, ended, last, now);
            sessions.total = sessions.total.saturating_add(1);
            let entry = per_agent_sessions.entry(agent).or_default();
            entry.0 = entry.0.saturating_add(1);
            match status {
                "active" => {
                    sessions.active = sessions.active.saturating_add(1);
                    entry.1 = entry.1.saturating_add(1);
                }
                "ended" => sessions.ended = sessions.ended.saturating_add(1),
                _ => sessions.unterminated = sessions.unterminated.saturating_add(1),
            }
        }
    }

    // One summary per known agent (even with no activity), plus any other
    // agent name that appears in the data.
    let mut names: Vec<String> = AgentKind::ALL
        .iter()
        .map(|k| k.as_str().to_string())
        .filter(|n| agents.is_empty() || agents.contains(n))
        .collect();
    for n in per_agent.keys().chain(per_agent_sessions.keys()) {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    let mut latest = c.prepare_cached(
        "SELECT session_key, last_started_at, ended_at, last_event_at FROM agent_sessions
         WHERE agent_name = ?1 AND last_seq > ?2 ORDER BY last_event_at DESC LIMIT 1",
    )?;
    let evicted = meta_get_i64(c, "evicted_through_seq")?;
    let mut summaries = Vec::with_capacity(names.len());
    for name in names {
        let newest: Option<(String, Option<i64>, Option<i64>, i64)> = latest
            .query_row(rusqlite::params![name, evicted], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let (sess, active) = per_agent_sessions.get(&name).copied().unwrap_or_default();
        summaries.push(AgentSummary {
            activity: per_agent.remove(&name).unwrap_or_default(),
            sessions: sess,
            active_sessions: active,
            latest_session_status: newest
                .as_ref()
                .map(|(_, s, e, l)| derive_status(*s, *e, *l, now)),
            latest_activity_at: newest
                .as_ref()
                .and_then(|(_, _, _, l)| DateTime::from_timestamp_micros(*l)),
            latest_session_id: newest.map(|(k, ..)| k),
            agent: name,
        });
    }

    let oldest: Option<(i64, i64)> = c
        .query_row(
            "SELECT seq, observed_at FROM events ORDER BY seq ASC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let truncated =
        meta_get_i64(c, "evicted_through_seq")? > 0 && oldest.is_some_and(|(_, at)| at > since);
    let gaps: i64 = c.query_row(
        "SELECT count(*) FROM gaps WHERE reason != 'session_deleted'
           AND ((to_seq >= ?1 AND from_seq <= ?2) OR (first_at <= ?4 AND last_at >= ?3))",
        rusqlite::params![lo.saturating_sub(1), hi, since, until],
        |r| r.get(0),
    )?;

    Ok(Stats {
        since: DateTime::from_timestamp_micros(since),
        until: DateTime::from_timestamp_micros(until),
        sessions,
        activity: totals,
        agents: summaries,
        events_ingested: to_u64(hi.saturating_sub(lo)),
        runtime_errors: runtime[0],
        runtime_warnings: runtime[1],
        bucket_secs: to_u64(width / 1_000_000),
        buckets,
        retention_truncated: truncated,
        history_starts_at: oldest.and_then(|(_, t)| DateTime::from_timestamp_micros(t)),
        gaps_in_range: to_u64(gaps),
    })
}

/// Filters for [`files`].
#[derive(Debug, Clone)]
pub struct FilesQuery {
    pub range: StatsQuery,
    pub sessions: Vec<String>,
    /// Prefix of the path (exact bytes).
    pub path_prefix: Option<String>,
    /// Only paths that more than one agent reported.
    pub multi_agent: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileActivity {
    pub path: String,
    pub project_id: Option<String>,
    pub events: u64,
    pub reads: u64,
    pub created: u64,
    pub modified: u64,
    pub deleted: u64,
    pub renamed: u64,
    /// Agents that reported an operation on this path.
    pub agents: Vec<String>,
    pub sessions: u64,
    pub last_seq: u64,
    pub last_event_type: String,
    pub last_at: Option<DateTime<Utc>>,
    /// Always `agent_reported` in this release: no filesystem watcher
    /// verified these operations.
    pub provenance: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilesPage {
    pub files: Vec<FileActivity>,
    /// Distinct paths matching the filters (may exceed `files.len()`).
    pub total_paths: u64,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// Per-path file activity reported by agents. Blocking.
///
/// # Errors
/// Invalid filters, the time budget, or a SQLite failure.
pub(crate) fn files(conn: &mut Connection, q: &FilesQuery) -> Result<FilesPage> {
    let (since, until, agents) = q.range.resolve()?;
    let limit = match q.limit {
        0 => DEFAULT_FILES_LIMIT,
        n if n > MAX_FILES_LIMIT => {
            return Err(invalid(format!("`limit` must be 1..={MAX_FILES_LIMIT}")))
        }
        n => n,
    };
    if q.sessions.len() > MAX_LIST_VALUES {
        return Err(invalid(format!(
            "`session` accepts at most {MAX_LIST_VALUES} values"
        )));
    }
    if q.path_prefix
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.len() > 1024)
    {
        return Err(invalid("`path_prefix` must be 1..=1024 bytes"));
    }
    let guarded = ProgressGuard::install(conn, Instant::now() + crate::query::QUERY_TIME_BUDGET)?;
    let c: &Connection = &guarded;
    let mut args: Vec<SqlValue> = vec![SqlValue::Integer(since), SqlValue::Integer(until)];
    // Index-only over `events_agent_activity` (event_type, ts, agent_name,
    // project_id, session_id, path): one range per file type, exact on event
    // time, no table lookups.
    let file_types: Vec<String> = EVENT_TYPES
        .iter()
        .filter(|t| t.name.starts_with("file."))
        .map(|t| format!("'{}'", t.name))
        .collect();
    let mut filter = format!(
        "event_type IN ({}) AND agent_name IS NOT NULL AND ts >= ?1 AND ts < ?2 AND path IS NOT NULL",
        file_types.join(", ")
    );
    push_in(&mut filter, &mut args, "project_id", &q.range.projects);
    push_in(&mut filter, &mut args, "agent_name", &agents);
    push_in(&mut filter, &mut args, "session_id", &q.sessions);
    if let Some(p) = &q.path_prefix {
        args.push(SqlValue::Text(p.clone()));
        let n = args.len();
        filter.push_str(&format!(" AND substr(path, 1, length(?{n})) = ?{n}"));
    }
    let having = if q.multi_agent {
        " HAVING count(DISTINCT agent_name) > 1"
    } else {
        ""
    };
    let total: i64 = c.query_row(
        &format!(
            "SELECT count(*) FROM (SELECT 1 FROM events INDEXED BY events_agent_activity WHERE {filter}
             GROUP BY path, project_id{having})"
        ),
        params_from_iter(args.iter()),
        |r| r.get(0),
    )?;
    let sql = format!(
        "SELECT path, project_id, count(*),
                sum(event_type = 'file.read'), sum(event_type = 'file.created'),
                sum(event_type = 'file.modified'), sum(event_type = 'file.deleted'),
                sum(event_type = 'file.renamed'),
                group_concat(DISTINCT agent_name), count(DISTINCT session_id),
                max(seq), max(ts)
         FROM events INDEXED BY events_agent_activity WHERE {filter}
         GROUP BY path, project_id{having}
         ORDER BY max(seq) DESC LIMIT {limit}"
    );
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
        let agents: Option<String> = r.get(8)?;
        let mut agents: Vec<String> = agents
            .unwrap_or_default()
            .split(',')
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        agents.sort();
        Ok(FileActivity {
            path: r.get(0)?,
            project_id: r.get(1)?,
            events: to_u64(r.get(2)?),
            reads: to_u64(r.get(3)?),
            created: to_u64(r.get(4)?),
            modified: to_u64(r.get(5)?),
            deleted: to_u64(r.get(6)?),
            renamed: to_u64(r.get(7)?),
            agents,
            sessions: to_u64(r.get(9)?),
            last_seq: to_u64(r.get(10)?),
            last_event_type: String::new(),
            last_at: DateTime::from_timestamp_micros(r.get(11)?),
            provenance: "agent_reported",
        })
    })?;
    let mut files: Vec<FileActivity> = rows.collect::<rusqlite::Result<_>>()?;
    let mut ty = c.prepare_cached("SELECT event_type FROM events WHERE seq = ?1")?;
    for f in &mut files {
        let seq = i64::try_from(f.last_seq).unwrap_or(i64::MAX);
        f.last_event_type = ty
            .query_row([seq], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
    }
    Ok(FilesPage {
        files,
        total_paths: to_u64(total),
        since: DateTime::from_timestamp_micros(since),
        until: DateTime::from_timestamp_micros(until),
    })
}
