//! Retention: age, payload age, and size cap.
//!
//! Correctness rules:
//! - **Deterministic expiry.** An event expires when its `observed_at` (when
//!   TailFlow received it) is older than `now - retention_days`. Because the
//!   writer keeps `observed_at` non-decreasing in `seq` order, expiry is always
//!   a prefix of the seq space: there is never a retained event older than a
//!   deleted one.
//! - **Oldest first under size pressure.** Size eviction deletes the lowest
//!   `seq` values, the same prefix.
//! - **Short transactions.** Work happens in chunks of [`CHUNK`] rows, each in
//!   its own `BEGIN IMMEDIATE` transaction, under a wall-clock budget, so the
//!   writer interleaves ingestion batches and readers never see a long lock.
//! - **Visible horizon.** `meta.evicted_through_seq` records the highest seq
//!   ever removed, so queries can say "history before this was expired"
//!   instead of implying it never existed.

use crate::error::Result;
use crate::schema::{meta_add, meta_get_i64, meta_max};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::time::{Duration, Instant};

/// Rows per delete transaction.
pub const CHUNK: i64 = 2_000;

/// Fraction of `max_bytes` that size eviction brings usage back down to, so a
/// database at the cap does not evict on every batch.
const SIZE_TARGET_NUM: u64 = 9;
const SIZE_TARGET_DEN: u64 = 10;

/// Free pages returned to the OS per maintenance pass (4 KiB pages → 4 MiB),
/// bounding write amplification and writer pause.
const VACUUM_PAGES_PER_PASS: u64 = 1_024;

/// Reclaim only once free pages exceed this many, or 5 % of the file. Under
/// ingestion FTS5 merges free pages continuously; chasing a zero freelist
/// would keep retention running forever (found by the M1 benchmark).
const VACUUM_MIN_FREE_PAGES: u64 = 2_048;

/// Pages of full-text index merging per maintenance step (≈ what one step
/// writes; measured 121 WAL pages median, 315 max on a 256 MB index).
const FTS_MERGE_PAGES: i64 = 64;

/// Tombstone share (%) above which a full-text segment level is eligible for
/// the targeted merge. Only set for the duration of that merge: left on, FTS5
/// would also run it inside ordinary commits, where M1 measured a 24 s COMMIT.
const FTS_DELETEMERGE_PERCENT: i64 = 10;

/// In a cleanup cycle that keeps running out of time (a large backlog), also
/// compact every this many passes instead of only when the cycle completes.
const FTS_COMPACT_EVERY_PASSES: u32 = 20;

/// What the earlier passes of the cleanup cycle in progress did. A cycle is
/// one pass, or several chained by `more_work`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cycle {
    /// An earlier pass of this cycle deleted events.
    pub deleted: bool,
    /// Passes of this cycle before the current one.
    pub passes: u32,
}

impl Cycle {
    /// The state for the next pass, given this pass's outcome.
    #[must_use]
    pub fn after(self, deleted: bool, more_work: bool) -> Self {
        if more_work {
            Self {
                deleted: self.deleted || deleted,
                passes: self.passes.saturating_add(1),
            }
        } else {
            Self::default()
        }
    }
}

/// Whether this pass ends with the targeted full-text compaction: the cycle
/// deleted events and either completes now or has run for another
/// `FTS_COMPACT_EVERY_PASSES` passes.
fn should_compact(cycle: Cycle, deleted_now: bool, more_work: bool) -> bool {
    (deleted_now || cycle.deleted)
        && (!more_work || cycle.passes % FTS_COMPACT_EVERY_PASSES == FTS_COMPACT_EVERY_PASSES - 1)
}

/// Merge the full-text segment levels that consist mostly of deleted rows'
/// tombstones, bounded to about `FTS_MERGE_PAGES` pages.
///
/// Retention deletes the oldest events; their index entries stay in the
/// oldest, largest segments as tombstones until those are merged. A positive
/// `merge` only touches a level holding `usermerge` (4) segments, which those
/// big old segments rarely reach, so under continuous eviction the index grew
/// to 3–13× its compacted size and took that space from the history the size
/// cap can hold (M1.5-F). With `deletemerge` set, a level whose segments are
/// over the tombstone threshold is merged even when alone. A *negative*
/// merge was rejected: it regroups every segment into one whole-index merge
/// that ordinary inserts then keep continuing through automerge, so its cost
/// grows with the index and lands on ingestion (≈3× slower cycles measured).
pub(crate) fn compact_fts(conn: &Connection) -> Result<()> {
    immediate(conn, |c| {
        c.execute(
            "INSERT INTO events_fts(events_fts, rank) VALUES ('deletemerge', ?1)",
            [FTS_DELETEMERGE_PERCENT],
        )?;
        c.execute(
            "INSERT INTO events_fts(events_fts, rank) VALUES ('merge', ?1)",
            [FTS_MERGE_PAGES],
        )?;
        c.execute(
            "INSERT INTO events_fts(events_fts, rank) VALUES ('deletemerge', 0)",
            [],
        )?;
        Ok(())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RetentionPolicy {
    /// `None` = unlimited age (size cap still applies).
    pub max_age_days: Option<u32>,
    pub payload_max_age_days: u32,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RetentionReport {
    pub expired_by_age: u64,
    pub evicted_by_size: u64,
    pub payloads_cleared: u64,
    pub vacuumed_pages: u64,
    pub duration_ms: f64,
    /// The time budget ran out with deletion work remaining; run again soon.
    /// Free-page reclamation never sets this.
    pub more_work: bool,
    /// This pass ran the bounded full-text compaction (the cleanup cycle had
    /// deleted events and completed, or ran long).
    pub fts_compacted: bool,
    /// The cycle owes a full-text compaction; the writer runs it unless the
    /// ingestion queue is deep (then it is deferred, see `writer`).
    #[serde(skip)]
    pub fts_compaction_due: bool,
    /// The pass stopped early because the ingestion queue was deep.
    pub yielded: bool,
    pub used_bytes_after: u64,
    pub age_ms: f64,
    pub payload_ms: f64,
    pub size_ms: f64,
    pub vacuum_ms: f64,
}

/// Page accounting for the main database file.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct PageStats {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
}

impl PageStats {
    pub fn used_bytes(&self) -> u64 {
        self.page_count
            .saturating_sub(self.freelist_count)
            .saturating_mul(self.page_size)
    }

    pub fn file_bytes(&self) -> u64 {
        self.page_count.saturating_mul(self.page_size)
    }
}

pub fn page_stats(conn: &Connection) -> Result<PageStats> {
    let get = |name: &str| -> Result<u64> {
        let v: i64 = conn.pragma_query_value(None, name, |r| r.get(0))?;
        Ok(u64::try_from(v).unwrap_or(0))
    };
    Ok(PageStats {
        page_size: get("page_size")?,
        page_count: get("page_count")?,
        freelist_count: get("freelist_count")?,
    })
}

/// Run `f` inside `BEGIN IMMEDIATE … COMMIT`, rolling back on error.
pub(crate) fn immediate<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(v) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(v),
            Err(e) => {
                let _rollback = conn.execute_batch("ROLLBACK");
                Err(e.into())
            }
        },
        Err(e) => {
            let _rollback = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// First seq with `observed_at >= cutoff_us`, or `max_seq + 1` when every
/// event is older. `None` when the table is empty.
pub(crate) fn first_seq_at_or_after(conn: &Connection, cutoff_us: i64) -> Result<Option<i64>> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT seq FROM events WHERE observed_at >= ?1 ORDER BY observed_at, seq LIMIT 1",
            [cutoff_us],
            |r| r.get(0),
        )
        .optional()?;
    if found.is_some() {
        return Ok(found);
    }
    let max: Option<i64> = conn.query_row("SELECT max(seq) FROM events", [], |r| r.get(0))?;
    Ok(max.map(|m| m.saturating_add(1)))
}

/// Delete one chunk of the oldest events with `seq < upto`. Returns rows
/// deleted (0 = nothing left below `upto`).
pub(crate) fn delete_oldest_chunk(conn: &Connection, upto: i64) -> Result<u64> {
    delete_oldest_rows(conn, upto, CHUNK)
}

/// Delete at most `rows` of the oldest events with `seq < upto`. Counted in
/// rows, not seq span, so sparse seq ranges (after session deletes) and very
/// large rows are both handled precisely.
pub(crate) fn delete_oldest_rows(conn: &Connection, upto: i64, rows: i64) -> Result<u64> {
    let rows = rows.clamp(1, CHUNK);
    immediate(conn, |c| {
        let lo: Option<i64> = c.query_row("SELECT min(seq) FROM events", [], |r| r.get(0))?;
        let Some(lo) = lo.filter(|lo| *lo < upto) else {
            return Ok(0);
        };
        let nth: Option<i64> = c
            .query_row(
                "SELECT seq FROM events ORDER BY seq LIMIT 1 OFFSET ?1",
                [rows - 1],
                |r| r.get(0),
            )
            .optional()?;
        let hi = nth.unwrap_or(i64::MAX).min(upto.saturating_sub(1));
        delete_seq_range(c, lo, hi)
    })
}

/// Delete `[lo, hi]` and maintain the FTS index, counters, horizon, and gap
/// rows that described the deleted history. Caller holds the transaction.
pub(crate) fn delete_seq_range(c: &Connection, lo: i64, hi: i64) -> Result<u64> {
    c.execute(
        "DELETE FROM events_fts WHERE rowid BETWEEN ?1 AND ?2",
        [lo, hi],
    )?;
    let n = c.execute("DELETE FROM events WHERE seq BETWEEN ?1 AND ?2", [lo, hi])?;
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    meta_add(c, "event_count", -n)?;
    meta_max(c, "evicted_through_seq", hi)?;
    meta_max(c, "payload_expired_through_seq", hi)?;
    crate::sessions::delete_expired(c, hi)?;
    // Strictly below `hi`: a loss anchored *at* the last deleted seq
    // describes events that would follow it, which may still be retained.
    c.execute("DELETE FROM gaps WHERE to_seq < ?1", [hi])?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// One retention pass under a time budget.
///
/// `cycle` describes the earlier passes of the cleanup cycle in progress; a
/// cycle that deleted events ends with a bounded full-text compaction
/// ([`compact_fts`]). Callers advance it with [`Cycle::after`].
pub fn run(
    conn: &Connection,
    policy: &RetentionPolicy,
    now_us: i64,
    budget: Duration,
    cycle: Cycle,
) -> Result<RetentionReport> {
    let mut report = run_with_pressure(conn, policy, now_us, budget, cycle, &|| false)?;
    if report.fts_compaction_due {
        match compact_fts(conn) {
            Ok(()) => report.fts_compacted = true,
            Err(e) => tracing::warn!(err = %e, "full-text compaction skipped"),
        }
    }
    Ok(report)
}

/// [`run`], yielding to ingestion: before every chunk, `under_pressure` is
/// asked whether the ingestion queue is deep; if so the pass stops with
/// `more_work` and `yielded` set and resumes after the next batch. The
/// routine full-text merge is skipped under pressure too. The writer
/// passes a closure that answers `false` once maintenance has been
/// deferred for too long, so cleanup always eventually progresses.
pub fn run_with_pressure(
    conn: &Connection,
    policy: &RetentionPolicy,
    now_us: i64,
    budget: Duration,
    cycle: Cycle,
    under_pressure: &dyn Fn() -> bool,
) -> Result<RetentionReport> {
    let started = Instant::now();
    let mut report = RetentionReport::default();
    let out_of_time = |r: &mut RetentionReport| {
        if started.elapsed() > budget {
            r.more_work = true;
            true
        } else if under_pressure() {
            r.more_work = true;
            r.yielded = true;
            true
        } else {
            false
        }
    };

    let phase = Instant::now();
    // 1. Age.
    if let Some(days) = policy.max_age_days {
        let cutoff = now_us.saturating_sub(i64::from(days).saturating_mul(86_400_000_000));
        if let Some(upto) = first_seq_at_or_after(conn, cutoff)? {
            loop {
                if out_of_time(&mut report) {
                    break;
                }
                let n = delete_oldest_chunk(conn, upto)?;
                if n == 0 {
                    break;
                }
                report.expired_by_age = report.expired_by_age.saturating_add(n);
            }
        }
    }

    report.age_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = Instant::now();
    // 2. Payload age.
    if !report.more_work {
        let days = i64::from(policy.payload_max_age_days);
        let cutoff = now_us.saturating_sub(days.saturating_mul(86_400_000_000));
        if let Some(upto) = first_seq_at_or_after(conn, cutoff)? {
            let mut from = meta_get_i64(conn, "payload_expired_through_seq")?.saturating_add(1);
            while from < upto {
                if out_of_time(&mut report) {
                    break;
                }
                let hi = from.saturating_add(CHUNK * 4 - 1).min(upto - 1);
                let n = immediate(conn, |c| {
                    let n = c.execute(
                        "UPDATE events SET payload = NULL
                         WHERE seq BETWEEN ?1 AND ?2 AND payload IS NOT NULL",
                        [from, hi],
                    )?;
                    meta_max(c, "payload_expired_through_seq", hi)?;
                    Ok(n)
                })?;
                report.payloads_cleared = report
                    .payloads_cleared
                    .saturating_add(u64::try_from(n).unwrap_or(0));
                from = hi.saturating_add(1);
            }
        }
    }

    report.payload_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = Instant::now();
    // 3. Size cap. Once over the cap, evict down to the 90 % target even if
    // that takes several passes (`size_evicting` remembers it across them).
    if !report.more_work {
        let target = policy.max_bytes / SIZE_TARGET_DEN * SIZE_TARGET_NUM;
        let used = page_stats(conn)?.used_bytes();
        let resuming = crate::schema::meta_get_i64(conn, "size_evicting")? == 1;
        if used > policy.max_bytes || (resuming && used > target) {
            crate::schema::meta_set(conn, "size_evicting", "1")?;
            loop {
                let used = page_stats(conn)?.used_bytes();
                if used <= target || out_of_time(&mut report) {
                    break;
                }
                // Estimate rows to remove from the average row footprint
                // (including indexes), so one chunk of very large rows cannot
                // overshoot the target and empty the database.
                let count = u64::try_from(meta_get_i64(conn, "event_count")?)
                    .unwrap_or(0)
                    .max(1);
                let per_row = (used / count).max(1);
                let needed = (used - target).div_ceil(per_row).max(1);
                let n = delete_oldest_rows(conn, i64::MAX, i64::try_from(needed).unwrap_or(CHUNK))?;
                if n == 0 {
                    break;
                }
                report.evicted_by_size = report.evicted_by_size.saturating_add(n);
            }
            if !report.more_work {
                crate::schema::meta_set(conn, "size_evicting", "0")?;
            }
        } else if resuming {
            crate::schema::meta_set(conn, "size_evicting", "0")?;
        }
    }
    report.size_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = Instant::now();

    // 4. Return a bounded number of free pages to the filesystem.
    let stats = page_stats(conn)?;
    let threshold = VACUUM_MIN_FREE_PAGES.max(stats.page_count / 20);
    if stats.freelist_count > threshold {
        let pages = VACUUM_PAGES_PER_PASS.min(stats.freelist_count);
        conn.execute_batch(&format!("PRAGMA incremental_vacuum({pages})"))?;
        let after = page_stats(conn)?.freelist_count;
        report.vacuumed_pages = stats.freelist_count.saturating_sub(after);
    }
    let deleted = report.expired_by_age + report.evicted_by_size > 0;
    // Full-text maintenance is best effort: the deletions above are already
    // committed, and a failed merge (e.g. BUSY from a concurrent CLI
    // command) must not fail the pass, drop its report or skip the
    // checkpoint below. The next cycle tries again.
    if deleted && !under_pressure() {
        // Routine bounded merging (~64 pages) of same-level segments.
        if let Err(e) = conn.execute(
            "INSERT INTO events_fts(events_fts, rank) VALUES ('merge', 64)",
            [],
        ) {
            tracing::warn!(err = %e, "full-text merge skipped");
        }
    }
    // The targeted compaction can take seconds on a bloated index (5.4 s
    // measured once on 2 GB); the writer decides when to run it.
    report.fts_compaction_due = should_compact(cycle, deleted, report.more_work);
    if report.expired_by_age + report.evicted_by_size + report.payloads_cleared > 0
        || report.vacuumed_pages > 0
    {
        // PASSIVE never waits on readers or blocks other writers. TRUNCATE
        // would invoke the busy handler for up to 5 s on the writer thread
        // and stall ingestion (found by the M1 benchmark); the WAL is still
        // trimmed to `journal_size_limit` when it next resets.
        let _checkpoint: rusqlite::Result<(i64, i64, i64)> =
            conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            });
    }
    report.vacuum_ms = phase.elapsed().as_secs_f64() * 1000.0;

    report.used_bytes_after = page_stats(conn)?.used_bytes();
    report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::open_writer;
    use tailflow_core::config::SyncMode;

    #[test]
    fn review_r3_gap_at_the_deletion_boundary_survives() {
        let dir = tempfile::tempdir().unwrap();
        let c = open_writer(&dir.path().join("e.db"), SyncMode::Normal).unwrap();
        for i in 1..=4 {
            c.execute(
                "INSERT INTO events (event_id, schema_version, ts, observed_at, event_type, provenance,
                 source_type, source_id, severity, run_id)
                 VALUES (?1, 1, ?2, ?2, 'runtime.log', 'runtime_observed', 'p', 's', 0, 1)",
                rusqlite::params![format!("e{i}"), i],
            )
            .unwrap();
        }
        // Loss detected right after seq 2 (lost events would follow it).
        c.execute(
            "INSERT INTO gaps (reason, count, first_at, last_at, from_seq, to_seq) VALUES ('queue_full', 3, 0, 0, 2, 2)",
            [],
        )
        .unwrap();
        immediate(&c, |c| delete_seq_range(c, 1, 2)).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM gaps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "seq 3.. is retained, so the gap still describes it");
        immediate(&c, |c| delete_seq_range(c, 3, 3)).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM gaps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "once history past it expires, the gap goes too");
    }

    #[test]
    fn maintenance_yields_under_queue_pressure_and_resumes_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let c = open_writer(&dir.path().join("e.db"), SyncMode::Normal).unwrap();
        immediate(&c, |c| {
            for i in 1..=100 {
                c.execute(
                    "INSERT INTO events (event_id, schema_version, ts, observed_at, event_type, provenance,
                     source_type, source_id, severity, run_id)
                     VALUES (?1, 1, ?2, ?2, 'runtime.log', 'runtime_observed', 'p', 's', 0, 1)",
                    rusqlite::params![format!("e{i}"), i],
                )?;
            }
            crate::schema::meta_add(c, "event_count", 100)?;
            Ok(())
        })
        .unwrap();
        let policy = RetentionPolicy {
            max_age_days: Some(1),
            payload_max_age_days: 1,
            max_bytes: u64::MAX,
        };
        let now = 10 * 86_400_000_000;
        let pressed = run_with_pressure(
            &c,
            &policy,
            now,
            Duration::from_secs(5),
            Cycle::default(),
            &|| true,
        )
        .unwrap();
        assert!(pressed.yielded && pressed.more_work);
        assert_eq!(
            pressed.expired_by_age, 0,
            "nothing deleted while the queue is deep"
        );
        let calm = run_with_pressure(
            &c,
            &policy,
            now,
            Duration::from_secs(5),
            Cycle::default(),
            &|| false,
        )
        .unwrap();
        assert!(!calm.yielded);
        assert_eq!(calm.expired_by_age, 100);
        assert!(
            calm.fts_compaction_due,
            "the writer owes a compaction after deleting"
        );
    }

    #[test]
    fn freelist_alone_never_keeps_retention_running() {
        let dir = tempfile::tempdir().unwrap();
        let c = open_writer(&dir.path().join("e.db"), SyncMode::Normal).unwrap();
        let policy = RetentionPolicy {
            max_age_days: None,
            payload_max_age_days: 7,
            max_bytes: u64::MAX,
        };
        let r = run(&c, &policy, 0, Duration::from_secs(1), Cycle::default()).unwrap();
        assert!(!r.more_work);
    }

    #[test]
    fn compaction_runs_once_a_deleting_cycle_completes_or_runs_long() {
        let none = Cycle::default();
        // Single-pass cycles.
        assert!(should_compact(none, true, false), "deleted and complete");
        assert!(!should_compact(none, false, false), "nothing deleted");
        assert!(!should_compact(none, true, true), "deleted, still running");
        // The completing pass deletes nothing, an earlier one did.
        let after_one = none.after(true, true);
        assert_eq!(
            after_one,
            Cycle {
                deleted: true,
                passes: 1
            }
        );
        assert!(should_compact(after_one, false, false));
        // A long backlog: every pass runs out of time.
        let mut c = none;
        let mut compactions = Vec::new();
        for pass in 0..45u32 {
            if should_compact(c, true, true) {
                compactions.push(pass);
            }
            c = c.after(true, true);
        }
        assert_eq!(compactions, vec![19, 39], "every 20th pass of a long cycle");
        // Completion resets the cycle.
        assert_eq!(c.after(false, false), Cycle::default());
        assert_eq!(
            Cycle {
                deleted: false,
                passes: u32::MAX
            }
            .after(false, true)
            .passes,
            u32::MAX
        );
    }

    #[test]
    fn compaction_leaves_deletemerge_off() {
        let dir = tempfile::tempdir().unwrap();
        let c = open_writer(&dir.path().join("e.db"), SyncMode::Normal).unwrap();
        for i in 1..=50 {
            c.execute(
                "INSERT INTO events_fts(rowid, message) VALUES (?1, 'alpha bravo')",
                [i],
            )
            .unwrap();
        }
        c.execute("DELETE FROM events_fts WHERE rowid <= 40", [])
            .unwrap();
        compact_fts(&c).unwrap();
        // Ordinary commits must never run tombstone merges themselves (M1:
        // a 24 s COMMIT): the setting is back at 0 after the step.
        let v: i64 = c
            .query_row(
                "SELECT v FROM events_fts_config WHERE k = 'deletemerge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 0);
        let live: i64 = c
            .query_row(
                "SELECT count(*) FROM events_fts WHERE events_fts MATCH 'alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, 10, "search sees exactly the live rows");
    }
}
