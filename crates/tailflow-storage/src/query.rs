//! Historical queries over persisted events.
//!
//! Ordering is by `seq` (ingestion order), never by timestamp, so pagination
//! is exact with identical timestamps and with late-arriving events. The
//! cursor is a seq: pass `next_cursor` back as `after` (ascending) or `before`
//! (descending).
//!
//! Work per request is bounded twice:
//! 1. **Seq windows.** The engine scans the seq space in windows of
//!    [`WINDOW`] and stops after a scan budget, returning `has_more` and a
//!    cursor at the exact boundary it reached. A sparse filter over months of
//!    history therefore costs several cheap round trips, never one unbounded
//!    scan, and the cursor always advances.
//! 2. **Wall clock.** A SQLite progress handler aborts any statement that runs
//!    past [`QUERY_TIME_BUDGET`].
//!
//! Every SQL value is a bound parameter.

use crate::error::{Result, StorageError};
use crate::retention::first_seq_at_or_after;
use crate::schema::meta_get_i64;
use chrono::{DateTime, Utc};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, OptionalExtension, Row};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tailflow_core::event::{EventStatus, ObservabilityEvent, Provenance, RedactionInfo};
use tailflow_core::LogLevel;

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1_000;
/// Max values in one list filter (`type=a,b,c`).
pub const MAX_LIST_VALUES: usize = 32;
pub const MAX_TEXT_TERMS: usize = 16;
pub const MAX_TERM_LEN: usize = 128;
/// Seq values scanned per statement.
pub const WINDOW: i64 = 65_536;
/// Seq span one request may scan when every filter is unindexed.
pub const SCAN_BUDGET_UNINDEXED: i64 = 1 << 20;
/// Seq span one request may scan when an indexed filter narrows each window.
pub const SCAN_BUDGET_INDEXED: i64 = 1 << 24;
pub const QUERY_TIME_BUDGET: Duration = Duration::from_secs(3);
/// Gaps listed in one response (the total is always reported).
pub const MAX_GAPS_LISTED: usize = 50;
/// `limit` cap when payload bodies are requested (each may be 256 KiB).
pub const MAX_LIMIT_WITH_PAYLOAD: usize = 100;
/// Approximate response budget: a page stops early (with `has_more`) once
/// the text it holds passes this, so no request can build a huge response.
pub const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

/// Structured query primitives. A future human query language
/// (`agent:claude type:file.modified since:24h`) compiles to this.
#[derive(Debug, Clone)]
pub struct EventQuery {
    /// Exact catalog types or `prefix.*`.
    pub event_types: Vec<String>,
    pub sources: Vec<String>,
    pub sessions: Vec<String>,
    pub agents: Vec<String>,
    pub projects: Vec<String>,
    pub min_severity: Option<LogLevel>,
    pub statuses: Vec<EventStatus>,
    /// `**` crosses `/`, `*` and `?` do not.
    pub path_glob: Option<String>,
    pub path_prefix: Option<String>,
    /// Substring of `attributes.command`.
    pub command: Option<String>,
    /// Exact `attributes."command.program"` (the program agents' command
    /// events report, e.g. `cargo`).
    pub program: Option<String>,
    /// Full-text terms, ANDed. A trailing `*` makes a term a prefix.
    pub text: Option<String>,
    /// On event `timestamp`.
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub after_seq: Option<u64>,
    pub before_seq: Option<u64>,
    pub order: Order,
    pub limit: usize,
    pub include_payload: bool,
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            event_types: Vec::new(),
            sources: Vec::new(),
            sessions: Vec::new(),
            agents: Vec::new(),
            projects: Vec::new(),
            min_severity: None,
            statuses: Vec::new(),
            path_glob: None,
            path_prefix: None,
            command: None,
            program: None,
            text: None,
            since: None,
            until: None,
            after_seq: None,
            before_seq: None,
            order: Order::Asc,
            limit: DEFAULT_LIMIT,
            include_payload: false,
        }
    }
}

fn invalid(msg: impl Into<String>) -> StorageError {
    StorageError::InvalidQuery(msg.into())
}

impl EventQuery {
    /// # Errors
    /// [`StorageError::InvalidQuery`] naming the offending field.
    pub fn validate(&self) -> Result<()> {
        if self.limit == 0 || self.limit > MAX_LIMIT {
            return Err(invalid(format!("`limit` must be 1..={MAX_LIMIT}")));
        }
        if self.include_payload && self.limit > MAX_LIMIT_WITH_PAYLOAD {
            return Err(invalid(format!(
                "`limit` must be 1..={MAX_LIMIT_WITH_PAYLOAD} with `include_payload`"
            )));
        }
        for (name, list) in [
            ("type", &self.event_types),
            ("source", &self.sources),
            ("session", &self.sessions),
            ("agent", &self.agents),
            ("project", &self.projects),
        ] {
            if list.len() > MAX_LIST_VALUES {
                return Err(invalid(format!(
                    "`{name}` accepts at most {MAX_LIST_VALUES} values"
                )));
            }
            if list.iter().any(|v| v.is_empty() || v.len() > 4096) {
                return Err(invalid(format!("`{name}` values must be 1..=4096 bytes")));
            }
        }
        if self.statuses.len() > MAX_LIST_VALUES {
            return Err(invalid("`status` accepts at most 32 values"));
        }
        for t in &self.event_types {
            let ok = match t.strip_suffix(".*") {
                Some(prefix) => {
                    !prefix.is_empty()
                        && tailflow_core::event::EVENT_TYPES
                            .iter()
                            .any(|s| s.name.starts_with(&format!("{prefix}.")))
                }
                None => tailflow_core::event::event_type_spec(t).is_some(),
            };
            if !ok {
                return Err(invalid(format!(
                    "unknown event type {t:?}; see GET /api/v1/schema for the catalog"
                )));
            }
        }
        if let Some(g) = &self.path_glob {
            tailflow_core::privacy::glob_to_regex(g).map_err(|e| invalid(e.to_string()))?;
        }
        for (name, v) in [
            ("path_prefix", &self.path_prefix),
            ("command", &self.command),
            ("program", &self.program),
        ] {
            if v.as_ref().is_some_and(|s| s.is_empty() || s.len() > 1024) {
                return Err(invalid(format!("`{name}` must be 1..=1024 bytes")));
            }
        }
        if let Some(t) = &self.text {
            fts_expression(t)?;
        }
        if let (Some(s), Some(u)) = (self.since, self.until) {
            if s >= u {
                return Err(invalid("`since` must be earlier than `until`"));
            }
        }
        if let (Some(a), Some(b)) = (self.after_seq, self.before_seq) {
            if a >= b {
                return Err(invalid("`after` must be less than `before`"));
            }
        }
        for (name, v) in [("after", self.after_seq), ("before", self.before_seq)] {
            if v.is_some_and(|n| i64::try_from(n).is_err()) {
                return Err(invalid(format!("`{name}` is out of range")));
            }
        }
        Ok(())
    }

    fn has_indexed_filter(&self) -> bool {
        !self.event_types.is_empty()
            || !self.sources.is_empty()
            || !self.sessions.is_empty()
            || !self.agents.is_empty()
            || !self.projects.is_empty()
            || self.text.is_some()
    }
}

/// Turn free text into a safe FTS5 expression: every term is a quoted phrase,
/// so FTS operators and syntax in user input are inert.
pub(crate) fn fts_expression(text: &str) -> Result<String> {
    let terms: Vec<&str> = text.split_whitespace().collect();
    if terms.is_empty() {
        return Err(invalid("`q` must contain at least one term"));
    }
    if terms.len() > MAX_TEXT_TERMS {
        return Err(invalid(format!(
            "`q` accepts at most {MAX_TEXT_TERMS} terms"
        )));
    }
    let mut out = Vec::with_capacity(terms.len());
    for term in terms {
        let (body, prefix) = match term.strip_suffix('*') {
            Some(b) if !b.is_empty() => (b, true),
            _ => (term, false),
        };
        if body.chars().count() > MAX_TERM_LEN || body.chars().any(char::is_control) {
            return Err(invalid(format!(
                "`q` terms must be 1..={MAX_TERM_LEN} characters without control characters"
            )));
        }
        let quoted = format!("\"{}\"", body.replace('"', "\"\""));
        out.push(if prefix { format!("{quoted}*") } else { quoted });
    }
    Ok(out.join(" "))
}

/// A persisted event with its cursor.
#[derive(Debug, Clone, Serialize)]
pub struct StoredEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub event: ObservabilityEvent,
}

#[derive(Debug, Clone, Serialize)]
pub struct GapView {
    pub reason: String,
    /// `None` = unknown number of events.
    pub count: Option<u64>,
    pub first_at: Option<DateTime<Utc>>,
    pub last_at: Option<DateTime<Utc>>,
    pub from_seq: u64,
    pub to_seq: u64,
}

/// Whether a page can be trusted as the whole story for its range.
#[derive(Debug, Clone, Serialize)]
pub struct Completeness {
    /// `false` whenever loss, deletion, or an index hole overlaps the scanned
    /// range. Never `true` after detected loss in that range.
    pub complete: bool,
    pub reasons: Vec<&'static str>,
    pub gaps: Vec<GapView>,
    pub gaps_total: u64,
    /// Losses detected in this run but not yet written as gap rows.
    pub pending_loss_unrecorded: u64,
    /// The requested range reaches back before retained history. Expired
    /// events existed; they are not evidence of absence.
    pub retention_truncated: bool,
    pub history_starts_at: Option<DateTime<Utc>>,
    pub oldest_retained_seq: Option<u64>,
    /// Highest seq ever removed by retention or purge (0 = none).
    pub evicted_through_seq: u64,
    /// Payload bodies at or below this seq were cleared by payload retention.
    pub payload_expired_through_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventPage {
    pub events: Vec<StoredEvent>,
    /// Resume point: pass as `after` (asc) or `before` (desc). Always set when
    /// anything was scanned, so a tailing client never rescans.
    pub next_cursor: Option<u64>,
    /// More matching events may exist beyond `next_cursor` in this direction.
    pub has_more: bool,
    /// The seq range actually examined.
    pub scanned_from_seq: Option<u64>,
    pub scanned_to_seq: Option<u64>,
    /// Stopped at the per-request scan budget, not at the end of the range.
    pub scan_budget_exhausted: bool,
    pub newest_seq: Option<u64>,
    pub completeness: Completeness,
}

pub(crate) struct QueryContext {
    pub fts_enabled: bool,
    pub fts_from_seq: i64,
    pub pending_loss: u64,
}

const COLUMNS: &str = "seq, event_id, dedup_key, schema_version, ts, observed_at, event_type, \
    provenance, source_type, source_id, project_id, session_id, operation_id, parent_operation_id, \
    agent_name, severity, status, duration_ms, path, message, attributes, {payload}, redaction";

fn columns(include_payload: bool) -> String {
    COLUMNS.replace(
        "{payload}",
        if include_payload { "payload" } else { "NULL" },
    )
}

fn conversion_err(idx: usize, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(msg),
    )
}

fn micros(idx: usize, v: i64) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_micros(v)
        .ok_or_else(|| conversion_err(idx, format!("timestamp {v} out of range")))
}

fn level_from_rank(rank: i64) -> LogLevel {
    match rank {
        1 => LogLevel::Trace,
        2 => LogLevel::Debug,
        3 => LogLevel::Info,
        4 => LogLevel::Warn,
        5 => LogLevel::Error,
        _ => LogLevel::Unknown,
    }
}

pub(crate) fn row_to_event(row: &Row<'_>) -> rusqlite::Result<StoredEvent> {
    let seq: i64 = row.get(0)?;
    let provenance: String = row.get(7)?;
    let status: Option<String> = row.get(16)?;
    let attributes: Option<String> = row.get(20)?;
    let redaction: Option<String> = row.get(22)?;
    let dedup_key: Option<String> = row.get(2)?;
    let event = ObservabilityEvent {
        event_id: row.get(1)?,
        stable_id: dedup_key.is_some(),
        schema_version: u16::try_from(row.get::<_, i64>(3)?)
            .map_err(|e| conversion_err(3, e.to_string()))?,
        timestamp: micros(4, row.get(4)?)?,
        observed_at: micros(5, row.get(5)?)?,
        event_type: row.get(6)?,
        provenance: Provenance::parse(&provenance)
            .ok_or_else(|| conversion_err(7, format!("unknown provenance {provenance:?}")))?,
        source_type: row.get(8)?,
        source_id: row.get(9)?,
        project_id: row.get(10)?,
        session_id: row.get(11)?,
        operation_id: row.get(12)?,
        parent_operation_id: row.get(13)?,
        agent_name: row.get(14)?,
        severity: level_from_rank(row.get(15)?),
        status: status.as_deref().and_then(EventStatus::parse),
        duration_ms: row
            .get::<_, Option<i64>>(17)?
            .and_then(|d| u64::try_from(d).ok()),
        path: row.get(18)?,
        message: row.get(19)?,
        attributes: match attributes {
            Some(s) => serde_json::from_str(&s).map_err(|e| conversion_err(20, e.to_string()))?,
            None => serde_json::Map::new(),
        },
        payload: row.get(21)?,
        redaction: match redaction {
            Some(s) => serde_json::from_str::<RedactionInfo>(&s)
                .map_err(|e| conversion_err(22, e.to_string()))?,
            None => RedactionInfo::default(),
        },
    };
    Ok(StoredEvent {
        seq: u64::try_from(seq).map_err(|e| conversion_err(0, e.to_string()))?,
        event,
    })
}

/// Fetch one event by seq.
pub(crate) fn get(
    conn: &Connection,
    seq: u64,
    include_payload: bool,
) -> Result<Option<StoredEvent>> {
    let Ok(seq) = i64::try_from(seq) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {} FROM events WHERE seq = ?1",
        columns(include_payload)
    );
    Ok(conn.query_row(&sql, [seq], row_to_event).optional()?)
}

struct Filters {
    sql: String,
    params: Vec<SqlValue>,
}

fn push_in(f: &mut Filters, column: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let mut placeholders = Vec::with_capacity(values.len());
    for v in values {
        f.params.push(SqlValue::Text(v.clone()));
        placeholders.push(format!("?{}", f.params.len() + 2));
    }
    f.sql
        .push_str(&format!(" AND {column} IN ({})", placeholders.join(", ")));
}

fn push_cond(f: &mut Filters, template: &str, value: SqlValue) {
    f.params.push(value);
    let n = f.params.len() + 2;
    f.sql.push_str(" AND ");
    f.sql.push_str(&template.replace("?N", &format!("?{n}")));
}

/// Build filter SQL. Placeholders `?1`/`?2` are the window bounds.
fn build_filters(q: &EventQuery) -> Result<Filters> {
    let mut f = Filters {
        sql: String::new(),
        params: Vec::new(),
    };
    let (exact, prefixes): (Vec<String>, Vec<String>) = q
        .event_types
        .iter()
        .cloned()
        .partition(|t| !t.ends_with(".*"));
    if !prefixes.is_empty() || !exact.is_empty() {
        let mut ors = Vec::new();
        for t in &exact {
            f.params.push(SqlValue::Text(t.clone()));
            ors.push(format!("event_type = ?{}", f.params.len() + 2));
        }
        for p in &prefixes {
            let prefix = p.trim_end_matches('*');
            // `prefix.` ≤ type < `prefix/` ('/' follows '.' in ASCII): an
            // index range, not a LIKE scan.
            f.params.push(SqlValue::Text(prefix.to_string()));
            let lo = f.params.len() + 2;
            f.params
                .push(SqlValue::Text(format!("{}/", prefix.trim_end_matches('.'))));
            let hi = f.params.len() + 2;
            ors.push(format!("(event_type >= ?{lo} AND event_type < ?{hi})"));
        }
        f.sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
    }
    push_in(&mut f, "source_id", &q.sources);
    push_in(&mut f, "session_id", &q.sessions);
    push_in(&mut f, "agent_name", &q.agents);
    push_in(&mut f, "project_id", &q.projects);
    if let Some(level) = q.min_severity {
        push_cond(
            &mut f,
            "severity >= ?N",
            SqlValue::Integer(i64::from(level.severity())),
        );
    }
    if !q.statuses.is_empty() {
        let values: Vec<String> = q.statuses.iter().map(|s| s.as_str().to_string()).collect();
        push_in(&mut f, "status", &values);
    }
    if let Some(prefix) = &q.path_prefix {
        push_cond(
            &mut f,
            "substr(path, 1, length(?N)) = ?N",
            SqlValue::Text(prefix.clone()),
        );
    }
    if q.path_glob.is_some() {
        f.sql.push_str(" AND tf_path_match(path)");
    }
    if let Some(cmd) = &q.command {
        push_cond(
            &mut f,
            "instr(json_extract(attributes, '$.command'), ?N) > 0",
            SqlValue::Text(cmd.clone()),
        );
    }
    if let Some(program) = &q.program {
        push_cond(
            &mut f,
            "json_extract(attributes, '$.\"command.program\"') = ?N",
            SqlValue::Text(program.clone()),
        );
    }
    if let Some(since) = q.since {
        push_cond(
            &mut f,
            "ts >= ?N",
            SqlValue::Integer(since.timestamp_micros()),
        );
    }
    if let Some(until) = q.until {
        push_cond(
            &mut f,
            "ts < ?N",
            SqlValue::Integer(until.timestamp_micros()),
        );
    }
    if let Some(text) = &q.text {
        push_cond(
            &mut f,
            "seq IN (SELECT rowid FROM events_fts WHERE events_fts MATCH ?N AND rowid BETWEEN ?1 AND ?2)",
            SqlValue::Text(fts_expression(text)?),
        );
    }
    Ok(f)
}

/// Run `q`.
pub(crate) fn execute(
    conn: &mut Connection,
    q: &EventQuery,
    ctx: &QueryContext,
) -> Result<EventPage> {
    q.validate()?;
    if q.text.is_some() && !ctx.fts_enabled {
        return Err(StorageError::FullTextDisabled);
    }

    // Wall-clock guard for every statement below. The guard removes the
    // handler on every exit path, including unwinding.
    let deadline = Instant::now() + QUERY_TIME_BUDGET;
    let guarded = ProgressGuard::install(conn, deadline)?;
    execute_bounded(&guarded, q, ctx)
}

fn execute_bounded(conn: &Connection, q: &EventQuery, ctx: &QueryContext) -> Result<EventPage> {
    let glob = q
        .path_glob
        .as_deref()
        .map(tailflow_core::privacy::glob_to_regex)
        .transpose()
        .map_err(|e| invalid(e.to_string()))?
        .map(Arc::new);
    if let Some(re) = glob.clone() {
        conn.create_scalar_function(
            "tf_path_match",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                let path: Option<String> = ctx.get(0)?;
                Ok(path.is_some_and(|p| re.is_match(&p.replace('\\', "/"))))
            },
        )?;
    }
    let _fn_guard = glob.as_ref().map(|_| FunctionGuard(conn));

    // Two single-aggregate lookups: SQLite optimizes `min(rowid)` or
    // `max(rowid)` alone to one b-tree probe, but `SELECT min(seq), max(seq)`
    // scans the whole table (~0.9 s at 3M rows, found by the M1 benchmark).
    let min_seq: Option<i64> = conn
        .query_row("SELECT seq FROM events ORDER BY seq ASC LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let max_seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;

    let mut page = EventPage {
        events: Vec::new(),
        next_cursor: None,
        has_more: false,
        scanned_from_seq: None,
        scanned_to_seq: None,
        scan_budget_exhausted: false,
        newest_seq: max_seq.and_then(|s| u64::try_from(s).ok()),
        completeness: base_completeness(conn, ctx)?,
    };

    let (Some(min_seq), Some(max_seq)) = (min_seq, max_seq) else {
        finish_completeness(conn, q, ctx, &mut page, None)?;
        return Ok(page);
    };

    let after = q.after_seq.map(|a| i64::try_from(a).unwrap_or(i64::MAX));
    let before = q.before_seq.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
    let mut lo = min_seq;
    let mut hi = max_seq;
    if let Some(a) = after {
        lo = lo.max(a.saturating_add(1));
    }
    if let Some(b) = before {
        hi = hi.min(b.saturating_sub(1));
    }
    if let Some(since) = q.since {
        // Exact: stored ts ≤ observed_at and observed_at is non-decreasing in
        // seq, so every event with ts ≥ since has seq ≥ this bound.
        if let Some(bound) = first_seq_at_or_after(conn, since.timestamp_micros())? {
            lo = lo.max(bound);
        }
    }
    if lo > hi {
        // Nothing to scan. Only a gap at the edge we would have resumed from
        // is relevant — not every gap ever recorded.
        let edge = lo.saturating_sub(1).min(max_seq);
        finish_completeness(conn, q, ctx, &mut page, Some(((edge + 1, edge), true)))?;
        // Nothing to scan; a tailing client should resume from the edge.
        page.next_cursor = match q.order {
            Order::Asc => u64::try_from(hi.max(lo.saturating_sub(1))).ok(),
            Order::Desc => u64::try_from(lo).ok(),
        };
        return Ok(page);
    }

    let filters = build_filters(q)?;
    let order = match q.order {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
    };
    let sql = format!(
        "SELECT {} FROM events WHERE seq BETWEEN ?1 AND ?2{} ORDER BY seq {order} LIMIT {}",
        columns(q.include_payload),
        filters.sql,
        q.limit + 1
    );
    let mut stmt = conn.prepare(&sql)?;
    let budget = if q.has_indexed_filter() {
        SCAN_BUDGET_INDEXED
    } else {
        SCAN_BUDGET_UNINDEXED
    };

    let mut scanned: i64 = 0;
    let mut page_bytes = 0usize;
    let mut cursor = match q.order {
        Order::Asc => lo,
        Order::Desc => hi,
    };
    let span: Option<(i64, i64)>;
    loop {
        let (wlo, whi) = match q.order {
            Order::Asc => (cursor, cursor.saturating_add(WINDOW - 1).min(hi)),
            Order::Desc => (cursor.saturating_sub(WINDOW - 1).max(lo), cursor),
        };
        let mut params: Vec<SqlValue> = vec![SqlValue::Integer(wlo), SqlValue::Integer(whi)];
        params.extend(filters.params.iter().cloned());
        let remaining = q.limit + 1 - page.events.len();
        let mut rows = stmt.query(params_from_iter(params.iter()))?;
        let mut taken = 0usize;
        let mut over_budget = false;
        while let Some(row) = rows.next()? {
            if taken == remaining {
                break;
            }
            let event = row_to_event(row)?;
            page_bytes = page_bytes.saturating_add(approx_bytes(&event));
            page.events.push(event);
            taken += 1;
            if page_bytes > MAX_PAGE_BYTES && page.events.len() <= q.limit {
                over_budget = true;
                break;
            }
        }
        drop(rows);
        if over_budget {
            // Byte budget reached before `limit`: stop at the last event.
            page.has_more = true;
            let last = page
                .events
                .last()
                .map_or(cursor, |e| i64::try_from(e.seq).unwrap_or(cursor));
            span = Some(match q.order {
                Order::Asc => (lo, last),
                Order::Desc => (last, hi),
            });
            page.next_cursor = u64::try_from(last).ok();
            break;
        }
        scanned = scanned.saturating_add(whi - wlo + 1);

        if page.events.len() > q.limit {
            // One extra row proves more matches exist; stop at the last
            // returned event so the next page starts exactly after it.
            page.events.truncate(q.limit);
            page.has_more = true;
            let last = page
                .events
                .last()
                .map_or(cursor, |e| i64::try_from(e.seq).unwrap_or(cursor));
            span = Some(match q.order {
                Order::Asc => (lo, last),
                Order::Desc => (last, hi),
            });
            page.next_cursor = u64::try_from(last).ok();
            break;
        }
        let at_end = match q.order {
            Order::Asc => whi >= hi,
            Order::Desc => wlo <= lo,
        };
        if at_end {
            span = Some((lo, hi));
            page.next_cursor = u64::try_from(match q.order {
                Order::Asc => hi,
                Order::Desc => lo,
            })
            .ok();
            break;
        }
        if scanned >= budget {
            page.has_more = true;
            page.scan_budget_exhausted = true;
            span = Some(match q.order {
                Order::Asc => (lo, whi),
                Order::Desc => (wlo, hi),
            });
            page.next_cursor = u64::try_from(match q.order {
                Order::Asc => whi,
                Order::Desc => wlo,
            })
            .ok();
            break;
        }
        cursor = match q.order {
            Order::Asc => whi + 1,
            Order::Desc => wlo - 1,
        };
    }
    if let Some((a, b)) = span {
        page.scanned_from_seq = u64::try_from(a).ok();
        page.scanned_to_seq = u64::try_from(b).ok();
    }
    let reached_tail = span.is_some_and(|(_, b)| b >= max_seq);
    finish_completeness(conn, q, ctx, &mut page, span.map(|s| (s, reached_tail)))?;
    Ok(page)
}

fn approx_bytes(e: &StoredEvent) -> usize {
    let ev = &e.event;
    256 + ev.message.as_ref().map_or(0, String::len)
        + ev.payload.as_ref().map_or(0, String::len)
        + ev.path.as_ref().map_or(0, String::len)
        + if ev.attributes.is_empty() {
            0
        } else {
            serde_json::to_vec(&ev.attributes).map_or(0, |v| v.len())
        }
}

fn base_completeness(conn: &Connection, ctx: &QueryContext) -> Result<Completeness> {
    let oldest: Option<(i64, i64)> = conn
        .query_row(
            "SELECT seq, observed_at FROM events ORDER BY seq ASC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Completeness {
        complete: true,
        reasons: Vec::new(),
        gaps: Vec::new(),
        gaps_total: 0,
        pending_loss_unrecorded: ctx.pending_loss,
        retention_truncated: false,
        history_starts_at: oldest.and_then(|(_, t)| DateTime::from_timestamp_micros(t)),
        oldest_retained_seq: oldest.and_then(|(s, _)| u64::try_from(s).ok()),
        evicted_through_seq: u64::try_from(meta_get_i64(conn, "evicted_through_seq")?).unwrap_or(0),
        payload_expired_through_seq: u64::try_from(meta_get_i64(
            conn,
            "payload_expired_through_seq",
        )?)
        .unwrap_or(0),
    })
}

/// `span` = scanned seq range and whether it reached the newest row.
fn finish_completeness(
    conn: &Connection,
    q: &EventQuery,
    ctx: &QueryContext,
    page: &mut EventPage,
    span: Option<((i64, i64), bool)>,
) -> Result<()> {
    let c = &mut page.completeness;
    let evicted = i64::try_from(c.evicted_through_seq).unwrap_or(0);

    // Gaps overlapping the scanned range (a loss recorded at seq N means
    // events may be missing right after N, so it touches [N, N+1]).
    let (glo, ghi, reached_tail) = match span {
        Some(((a, b), tail)) => (a.saturating_sub(1), b, tail),
        // Nothing scanned: still report gaps after the cursor or the tail.
        None => {
            let a = q
                .after_seq
                .map_or(0, |v| i64::try_from(v).unwrap_or(i64::MAX));
            (a, i64::MAX, true)
        }
    };
    c.gaps_total = u64::try_from(conn.query_row(
        "SELECT count(*) FROM gaps WHERE to_seq >= ?1 AND from_seq <= ?2",
        [glo, ghi],
        |r| r.get::<_, i64>(0),
    )?)
    .unwrap_or(0);
    if c.gaps_total > 0 {
        let mut stmt = conn.prepare_cached(
            "SELECT reason, count, first_at, last_at, from_seq, to_seq FROM gaps
             WHERE to_seq >= ?1 AND from_seq <= ?2 ORDER BY from_seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![glo, ghi, i64::try_from(MAX_GAPS_LISTED).unwrap_or(50)],
            |r| {
                Ok(GapView {
                    reason: r.get(0)?,
                    count: r
                        .get::<_, Option<i64>>(1)?
                        .and_then(|n| u64::try_from(n).ok()),
                    first_at: DateTime::from_timestamp_micros(r.get(2)?),
                    last_at: DateTime::from_timestamp_micros(r.get(3)?),
                    from_seq: u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                    to_seq: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
                })
            },
        )?;
        for g in rows {
            c.gaps.push(g?);
        }
        c.complete = false;
        if c.gaps.iter().any(|g| g.reason == "session_deleted") {
            c.reasons.push("deleted_in_range");
        }
        if c.gaps.iter().any(|g| g.reason != "session_deleted") || c.gaps.is_empty() {
            c.reasons.push("loss_in_range");
        }
    }
    if ctx.pending_loss > 0 && reached_tail {
        c.complete = false;
        c.reasons.push("pending_loss");
    }
    if q.text.is_some() {
        let scan_lo = span.map_or(0, |((a, _), _)| a);
        if ctx.fts_from_seq > scan_lo.max(1) {
            c.complete = false;
            c.reasons.push("text_index_incomplete");
        }
    }
    if evicted > 0 {
        let since_before_horizon = match (q.since, c.history_starts_at) {
            (Some(since), Some(start)) => since < start,
            (Some(_), None) => true,
            (None, _) => q.after_seq.is_none(),
        };
        let cursor_behind = q
            .after_seq
            .is_some_and(|a| i64::try_from(a).unwrap_or(i64::MAX) < evicted);
        if cursor_behind {
            // A follower's sequence now has a hole: events it never saw
            // were expired before it read them.
            c.complete = false;
            c.reasons.push("cursor_behind_retention");
        }
        c.retention_truncated = since_before_horizon || cursor_behind;
    }
    Ok(())
}

/// Holds the connection exclusively while a deadline progress handler is
/// installed (rusqlite ≥ 0.41 requires `&mut` to change the handler); derefs
/// to the connection for the query itself and removes the handler on drop.
pub(crate) struct ProgressGuard<'a>(&'a mut Connection);

impl<'a> ProgressGuard<'a> {
    pub(crate) fn install(conn: &'a mut Connection, deadline: Instant) -> Result<Self> {
        conn.progress_handler(10_000, Some(move || Instant::now() > deadline))?;
        Ok(Self(conn))
    }
}

impl std::ops::Deref for ProgressGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.0
    }
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        let _unset: rusqlite::Result<()> = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

struct FunctionGuard<'a>(&'a Connection);

impl Drop for FunctionGuard<'_> {
    fn drop(&mut self) {
        let _removed: rusqlite::Result<()> = self.0.remove_function("tf_path_match", 1);
    }
}

/// Events of one session in a time range whose operation ids link them to
/// an operation: the same operation, its children, its parent, or its
/// siblings (`operation_id` / `parent_operation_id` equal to `op` or
/// `parent`). Matched in SQL through the session index, under the query
/// time budget. Returns at most `limit` events (ascending time) and the
/// number that matched.
///
/// # Errors
/// SQLite failure or the time budget ([`StorageError::QueryTimeout`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn operation_links(
    conn: &mut Connection,
    session: &str,
    op: Option<&str>,
    parent: Option<&str>,
    exclude_seq: u64,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    limit: usize,
) -> Result<(Vec<StoredEvent>, u64)> {
    if op.is_none() && parent.is_none() {
        return Ok((Vec::new(), 0));
    }
    let deadline = Instant::now() + QUERY_TIME_BUDGET;
    let guarded = ProgressGuard::install(conn, deadline)?;
    let filter = "FROM events INDEXED BY events_session
         WHERE session_id = ?1 AND ts BETWEEN ?2 AND ?3 AND seq != ?4
           AND (operation_id IN (?5, ?6) OR parent_operation_id IN (?5, ?6))";
    let exclude = i64::try_from(exclude_seq).unwrap_or(i64::MAX);
    let since = since.timestamp_micros();
    let until = until.timestamp_micros();
    let total: i64 = guarded.query_row(
        &format!("SELECT count(*) {filter}"),
        rusqlite::params![session, since, until, exclude, op, parent],
        |r| r.get(0),
    )?;
    let mut stmt = guarded.prepare(&format!(
        "SELECT {} {filter} ORDER BY ts, seq LIMIT ?7",
        columns(false)
    ))?;
    let rows = stmt.query_map(
        rusqlite::params![
            session,
            since,
            until,
            exclude,
            op,
            parent,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ],
        row_to_event,
    )?;
    let events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((events, u64::try_from(total).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_expression_neutralises_operators() {
        assert_eq!(
            fts_expression("connection refused").unwrap(),
            "\"connection\" \"refused\""
        );
        assert_eq!(
            fts_expression("NEAR(a b) OR x").unwrap(),
            "\"NEAR(a\" \"b)\" \"OR\" \"x\""
        );
        assert_eq!(
            fts_expression("say \"hi\"").unwrap(),
            "\"say\" \"\"\"hi\"\"\""
        );
        assert_eq!(fts_expression("post*").unwrap(), "\"post\"*");
        assert!(fts_expression("   ").is_err());
        assert!(fts_expression(&"a ".repeat(17)).is_err());
    }

    #[test]
    fn validation_rejects_bad_input() {
        let bad = [
            EventQuery {
                limit: 0,
                ..Default::default()
            },
            EventQuery {
                limit: MAX_LIMIT + 1,
                ..Default::default()
            },
            EventQuery {
                event_types: vec!["file.modifed".into()],
                ..Default::default()
            },
            EventQuery {
                event_types: vec!["nope.*".into()],
                ..Default::default()
            },
            EventQuery {
                path_glob: Some(String::new()),
                ..Default::default()
            },
            EventQuery {
                after_seq: Some(5),
                before_seq: Some(5),
                ..Default::default()
            },
            EventQuery {
                after_seq: Some(u64::MAX),
                ..Default::default()
            },
            EventQuery {
                sources: (0..40).map(|i| i.to_string()).collect(),
                ..Default::default()
            },
        ];
        for q in bad {
            assert!(q.validate().is_err(), "{q:?}");
        }
        EventQuery {
            event_types: vec!["file.*".into(), "runtime.error".into()],
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
}
