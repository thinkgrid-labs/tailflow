//! File-change evidence (M4-C): agent-reported file operations compared
//! with independent filesystem observations.
//!
//! Every item gets exactly one status:
//!
//! | Status | Meaning |
//! |---|---|
//! | `matched_observation` | An agent reported changing the path, and the filesystem observer saw that path change within ± `window`. A stronger association than time alone; still **not proof of authorship** (another writer could have changed it in the same window). |
//! | `reported_only` | Reported, and the path was under a healthy observer at that time (full coverage, no recorded observer loss), but no change was seen within the window: a no-op write, a failed edit the agent still reported, or a change outside the window. |
//! | `unverified` | Reported, but it could not be checked: no observer covered the path then, coverage was partial (Linux watch budget), or observer loss overlaps the window. Absence of an observation means nothing here. |
//! | `filesystem_observed` | The observer saw the path change and no agent reported it within the window: a human, a build, a formatter, or an agent without hooks. Unattributed. |
//! | `ambiguous` | One observed change is matched by reports from more than one session. It cannot be credited to either. |
//!
//! Queries are bounded: a time range, at most [`MAX_ITEMS`] items per side,
//! a few indexed lookups per item (`events_path_ts`), a window ≤ 60 s.

use crate::error::{Result, StorageError};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

pub const MAX_ITEMS: usize = 500;
pub const MAX_WINDOW: Duration = Duration::from_secs(60);
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(5);
const CHANGE_TYPES: &str = "'file.created','file.modified','file.deleted','file.renamed'";
/// Loss reasons that mean the observer may have missed changes.
const OBSERVER_LOSS: &[&str] = &[
    "fs_observer_overflow",
    "fs_watcher_rescan",
    "unclean_shutdown",
];

#[derive(Debug, Clone)]
pub struct FileEvidenceQuery {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub session: Option<String>,
    pub agent: Option<String>,
    /// Observed paths under this prefix (and reports under it).
    pub path_prefix: Option<String>,
    pub window: Duration,
    pub limit: usize,
}

impl FileEvidenceQuery {
    /// # Errors
    /// Out-of-range window or limit, inverted range.
    pub fn validate(&self) -> Result<()> {
        if self.since >= self.until {
            return Err(StorageError::InvalidQuery(
                "`since` must be earlier than `until`".into(),
            ));
        }
        if self.window.is_zero() || self.window > MAX_WINDOW {
            return Err(StorageError::InvalidQuery(
                "`window_ms` must be 1..=60000".into(),
            ));
        }
        if self.limit == 0 || self.limit > MAX_ITEMS {
            return Err(StorageError::InvalidQuery(format!(
                "`limit` must be 1..={MAX_ITEMS}"
            )));
        }
        for v in [&self.session, &self.agent, &self.path_prefix]
            .into_iter()
            .flatten()
        {
            if v.is_empty() || v.len() > 4096 {
                return Err(StorageError::InvalidQuery(
                    "filter values must be 1..=4096 bytes".into(),
                ));
            }
        }
        Ok(())
    }
}

/// One event referenced by an evidence item.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceRef {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEvidence {
    pub path: String,
    pub at: DateTime<Utc>,
    pub status: &'static str,
    /// Why this status, in words (also lists the limitation that applies).
    pub explanation: String,
    pub reported: Vec<EvidenceRef>,
    pub observed: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageView {
    pub root: String,
    pub started_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub backend: String,
    pub watched_dirs: u64,
    pub unwatched_dirs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObserverLoss {
    pub reason: String,
    pub count: Option<u64>,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEvidencePage {
    pub items: Vec<FileEvidence>,
    pub counts: BTreeMap<&'static str, u64>,
    pub window_ms: u64,
    /// Observation coverage overlapping the range.
    pub coverage: Vec<CoverageView>,
    /// Observer loss overlapping the range.
    pub loss: Vec<ObserverLoss>,
    /// More reported or observed changes exist in the range than were
    /// compared (narrow the range or filters).
    pub truncated: bool,
}

fn micros(t: DateTime<Utc>) -> i64 {
    t.timestamp_micros()
}

fn from_micros(v: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_micros(v).unwrap_or_default()
}

fn under(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
}

struct Row {
    seq: i64,
    ts: i64,
    ty: String,
    path: String,
    session: Option<String>,
    agent: Option<String>,
}

impl Row {
    fn as_ref(&self) -> EvidenceRef {
        EvidenceRef {
            seq: u64::try_from(self.seq).unwrap_or(0),
            at: from_micros(self.ts),
            event_type: self.ty.clone(),
            session_id: self.session.clone(),
            agent_name: self.agent.clone(),
        }
    }
}

fn path_rows(c: &Connection, source: &str, path: &str, lo: i64, hi: i64) -> Result<Vec<Row>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT seq, ts, event_type, path, session_id, agent_name FROM events INDEXED BY events_path_ts
         WHERE path = ?1 AND ts BETWEEN ?2 AND ?3
           AND path IS NOT NULL AND source_type IN ('fs_watcher', 'agent_hook')
           AND source_type = ?4 AND event_type IN ({CHANGE_TYPES})
         ORDER BY ts LIMIT 16"
    ))?;
    let rows = stmt.query_map(params![path, lo, hi, source], |r| {
        Ok(Row {
            seq: r.get(0)?,
            ts: r.get(1)?,
            ty: r.get(2)?,
            path: r.get(3)?,
            session: r.get(4)?,
            agent: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `LIKE` pattern matching `prefix` literally, then anything.
fn like_prefix(prefix: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(ch);
    }
    s.push('%');
    s
}

/// Compute file evidence for a range, under the query time budget.
///
/// # Errors
/// Invalid query, time budget, or SQLite failure.
pub fn files(conn: &mut Connection, q: &FileEvidenceQuery) -> Result<FileEvidencePage> {
    q.validate()?;
    let guarded = crate::query::ProgressGuard::install(
        conn,
        std::time::Instant::now() + crate::query::QUERY_TIME_BUDGET,
    )?;
    files_inner(&guarded, q)
}

fn files_inner(c: &Connection, q: &FileEvidenceQuery) -> Result<FileEvidencePage> {
    let w = i64::try_from(q.window.as_micros()).unwrap_or(i64::MAX);
    let (since, until) = (micros(q.since), micros(q.until));

    // Coverage and observer loss over the range (both small tables).
    let coverage: Vec<CoverageView> = {
        let mut stmt = c.prepare_cached(
            "SELECT root, started_at, stopped_at, backend, watched_dirs, unwatched_dirs FROM fs_coverage
             WHERE started_at <= ?2 AND (stopped_at IS NULL OR stopped_at >= ?1)
             ORDER BY started_at LIMIT 1000",
        )?;
        let rows = stmt.query_map(params![since - w, until + w], |r| {
            Ok(CoverageView {
                root: r.get(0)?,
                started_at: from_micros(r.get(1)?),
                stopped_at: r.get::<_, Option<i64>>(2)?.map(from_micros),
                backend: r.get(3)?,
                watched_dirs: u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                unwatched_dirs: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let loss: Vec<ObserverLoss> = {
        let placeholders = OBSERVER_LOSS
            .iter()
            .map(|r| format!("'{r}'"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = c.prepare_cached(&format!(
            "SELECT reason, count, first_at, last_at FROM gaps
             WHERE reason IN ({placeholders}) AND first_at <= ?2 AND last_at >= ?1
             ORDER BY first_at LIMIT 1000"
        ))?;
        let rows = stmt.query_map(params![since - w, until + w], |r| {
            Ok(ObserverLoss {
                reason: r.get(0)?,
                count: r
                    .get::<_, Option<i64>>(1)?
                    .and_then(|n| u64::try_from(n).ok()),
                first_at: from_micros(r.get(2)?),
                last_at: from_micros(r.get(3)?),
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let covered = |path: &str, ts: i64| -> Result<(), &'static str> {
        let mut any = false;
        let mut partial = false;
        for cv in &coverage {
            let start = micros(cv.started_at);
            let stop = cv.stopped_at.map_or(i64::MAX, micros);
            // Covered at the reported moment. (Loss is checked over the
            // whole window below.)
            if under(path, &cv.root) && start <= ts && stop >= ts {
                any = true;
                partial |= cv.unwatched_dirs > 0;
            }
        }
        if !any {
            return Err("no filesystem observer covered this path at the time");
        }
        if partial {
            return Err("observer coverage was partial (directory watch budget exceeded)");
        }
        let (lo, hi) = (ts.saturating_sub(w), ts.saturating_add(w));
        if loss
            .iter()
            .any(|l| micros(l.first_at) <= hi && micros(l.last_at) >= lo)
        {
            return Err("recorded observer loss overlaps the window");
        }
        Ok(())
    };

    let limit = i64::try_from(q.limit).unwrap_or(500);
    // Reported changes (agent hooks), newest first, through the agent
    // activity index.
    let reported: Vec<Row> = {
        let mut sql = format!(
            "SELECT seq, ts, event_type, path, session_id, agent_name FROM events INDEXED BY events_agent_activity
             WHERE agent_name IS NOT NULL AND event_type IN ({CHANGE_TYPES})
               AND ts BETWEEN ?1 AND ?2 AND source_type = 'agent_hook' AND path IS NOT NULL"
        );
        let mut args: Vec<rusqlite::types::Value> = vec![since.into(), until.into()];
        if let Some(s) = &q.session {
            args.push(s.clone().into());
            sql.push_str(&format!(" AND session_id = ?{}", args.len()));
        }
        if let Some(a) = &q.agent {
            args.push(a.clone().into());
            sql.push_str(&format!(" AND agent_name = ?{}", args.len()));
        }
        if let Some(p) = &q.path_prefix {
            args.push(like_prefix(p).into());
            sql.push_str(&format!(" AND path LIKE ?{} ESCAPE '\\'", args.len()));
        }
        args.push((limit + 1).into());
        sql.push_str(&format!(" ORDER BY ts DESC LIMIT ?{}", args.len()));
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok(Row {
                seq: r.get(0)?,
                ts: r.get(1)?,
                ty: r.get(2)?,
                path: r.get(3)?,
                session: r.get(4)?,
                agent: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    // Observed changes, newest first. Session/agent filters do not apply
    // to observations (they have none); a session filter instead limits
    // observations to paths that session reported.
    let observed: Vec<Row> = if q.session.is_some() || q.agent.is_some() {
        Vec::new()
    } else {
        let mut sql =
            "SELECT seq, ts, event_type, path, NULL, NULL FROM events INDEXED BY events_fs_ts
             WHERE source_type = 'fs_watcher' AND ts BETWEEN ?1 AND ?2 AND path IS NOT NULL"
                .to_string();
        let mut args: Vec<rusqlite::types::Value> = vec![since.into(), until.into()];
        if let Some(p) = &q.path_prefix {
            args.push(like_prefix(p).into());
            sql.push_str(&format!(" AND path LIKE ?{} ESCAPE '\\'", args.len()));
        }
        args.push((limit + 1).into());
        sql.push_str(&format!(" ORDER BY ts DESC LIMIT ?{}", args.len()));
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok(Row {
                seq: r.get(0)?,
                ts: r.get(1)?,
                ty: r.get(2)?,
                path: r.get(3)?,
                session: None,
                agent: None,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let truncated = reported.len() > q.limit || observed.len() > q.limit;

    let mut items = Vec::new();
    let mut explained_obs: HashSet<i64> = HashSet::new();
    for r in reported.iter().take(q.limit) {
        let obs = path_rows(c, "fs_watcher", &r.path, r.ts - w, r.ts + w)?;
        let (status, explanation) = if obs.is_empty() {
            match covered(&r.path, r.ts) {
                Ok(()) => (
                    "reported_only",
                    "reported by the agent; the path was observed with full coverage and no loss, \
                     but no change was seen within the window"
                        .to_string(),
                ),
                Err(why) => (
                    "unverified",
                    format!("reported by the agent; cannot be checked: {why}"),
                ),
            }
        } else {
            // Did another session also report this same observed change?
            let mut sessions: HashSet<Option<String>> = HashSet::new();
            sessions.insert(r.session.clone());
            for o in &obs {
                for other in path_rows(c, "agent_hook", &r.path, o.ts - w, o.ts + w)? {
                    sessions.insert(other.session.clone());
                }
            }
            for o in &obs {
                explained_obs.insert(o.seq);
            }
            if sessions.len() > 1 {
                (
                    "ambiguous",
                    format!(
                        "the observed change is matched by reports from {} sessions; it cannot be credited to one",
                        sessions.len()
                    ),
                )
            } else {
                (
                    "matched_observation",
                    "reported by the agent and independently observed within the window \
                     (association, not proof of authorship)"
                        .to_string(),
                )
            }
        };
        items.push(FileEvidence {
            path: r.path.clone(),
            at: from_micros(r.ts),
            status,
            explanation,
            reported: vec![r.as_ref()],
            observed: obs.iter().map(Row::as_ref).collect(),
        });
    }
    for o in observed.iter().take(q.limit) {
        if explained_obs.contains(&o.seq) {
            continue;
        }
        let reports = path_rows(c, "agent_hook", &o.path, o.ts - w, o.ts + w)?;
        let sessions: HashSet<Option<String>> = reports.iter().map(|r| r.session.clone()).collect();
        let (status, explanation) = match sessions.len() {
            0 => (
                "filesystem_observed",
                "observed on disk; no agent reported this path within the window (unattributed: \
                 a person, a build or tool, or an agent without hooks)"
                    .to_string(),
            ),
            1 => (
                "matched_observation",
                "observed on disk and reported by one agent session within the window \
                 (association, not proof of authorship)"
                    .to_string(),
            ),
            n => (
                "ambiguous",
                format!("reported by {n} sessions within the window; it cannot be credited to one"),
            ),
        };
        if status == "matched_observation" {
            // Already listed from the report side when the report is in
            // range; otherwise list it here.
            if reports.iter().any(|r| r.ts >= since && r.ts <= until) {
                continue;
            }
        }
        items.push(FileEvidence {
            path: o.path.clone(),
            at: from_micros(o.ts),
            status,
            explanation,
            reported: reports.iter().map(Row::as_ref).collect(),
            observed: vec![o.as_ref()],
        });
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.at));
    let mut counts = BTreeMap::new();
    for s in [
        "matched_observation",
        "reported_only",
        "unverified",
        "filesystem_observed",
        "ambiguous",
    ] {
        counts.insert(s, 0u64);
    }
    for i in &items {
        *counts.entry(i.status).or_insert(0) += 1;
    }
    Ok(FileEvidencePage {
        items,
        counts,
        window_ms: u64::try_from(q.window.as_millis()).unwrap_or(0),
        coverage,
        loss,
        truncated,
    })
}

/// File-change events at exactly `path` between `since` and `until`, from
/// `source_type` (`fs_watcher` or `agent_hook`), oldest first, at most
/// `limit` (≤ 50). Uses the `events_path_ts` index.
///
/// # Errors
/// SQLite failure.
pub fn events_at_path(
    c: &Connection,
    path: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    source_type: &str,
    limit: usize,
) -> Result<Vec<EvidenceRef>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT seq, ts, event_type, path, session_id, agent_name FROM events INDEXED BY events_path_ts
         WHERE path = ?1 AND ts BETWEEN ?2 AND ?3
           AND path IS NOT NULL AND source_type IN ('fs_watcher', 'agent_hook')
           AND source_type = ?4 AND event_type IN ({CHANGE_TYPES})
         ORDER BY ts LIMIT ?5"
    ))?;
    let rows = stmt.query_map(
        params![
            path,
            micros(since),
            micros(until),
            source_type,
            i64::try_from(limit.min(50)).unwrap_or(50)
        ],
        |r| {
            Ok(Row {
                seq: r.get(0)?,
                ts: r.get(1)?,
                ty: r.get(2)?,
                path: r.get(3)?,
                session: r.get(4)?,
                agent: r.get(5)?,
            })
        },
    )?;
    let rows: Vec<Row> = rows.collect::<rusqlite::Result<_>>()?;
    Ok(rows.iter().map(Row::as_ref).collect())
}

/// The most recent `git.snapshot` event seq for a worktree root (its
/// `path`) at or before `at`. Walks `git.snapshot` rows newest first
/// through the type index.
pub fn latest_snapshot_before(
    c: &Connection,
    root: &str,
    at: DateTime<Utc>,
) -> Result<Option<u64>> {
    let seq: Option<i64> = c
        .query_row(
            "SELECT seq FROM events INDEXED BY events_type
             WHERE event_type = 'git.snapshot' AND path = ?1 AND ts <= ?2
             ORDER BY seq DESC LIMIT 1",
            params![root, micros(at)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(seq.and_then(|s| u64::try_from(s).ok()))
}
