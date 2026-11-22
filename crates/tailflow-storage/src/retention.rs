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
pub const CHUNK: i64 = 5_000;

/// Fraction of `max_bytes` that size eviction brings usage back down to, so a
/// database at the cap does not evict on every batch.
const SIZE_TARGET_NUM: u64 = 9;
const SIZE_TARGET_DEN: u64 = 10;

/// Free pages returned to the OS per maintenance pass (4 KiB pages → 16 MiB),
/// bounding write amplification.
const VACUUM_PAGES_PER_PASS: i64 = 4_096;

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
    /// The time budget ran out with work remaining; run again soon.
    pub more_work: bool,
    pub used_bytes_after: u64,
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
    c.execute("DELETE FROM gaps WHERE to_seq <= ?1", [hi])?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// One retention pass under a time budget.
pub fn run(
    conn: &Connection,
    policy: &RetentionPolicy,
    now_us: i64,
    budget: Duration,
) -> Result<RetentionReport> {
    let started = Instant::now();
    let mut report = RetentionReport::default();
    let out_of_time = |r: &mut RetentionReport| {
        if started.elapsed() > budget {
            r.more_work = true;
            true
        } else {
            false
        }
    };

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

    // 3. Size cap.
    if !report.more_work {
        let target = policy.max_bytes / SIZE_TARGET_DEN * SIZE_TARGET_NUM;
        if page_stats(conn)?.used_bytes() > policy.max_bytes {
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
        }
    }

    // 4. Return a bounded number of free pages to the filesystem.
    let before = page_stats(conn)?.freelist_count;
    if before > 0 {
        conn.execute_batch(&format!(
            "PRAGMA incremental_vacuum({VACUUM_PAGES_PER_PASS})"
        ))?;
        let after = page_stats(conn)?.freelist_count;
        report.vacuumed_pages = before.saturating_sub(after);
        if after > 0 {
            report.more_work = true;
        }
    }
    if report.expired_by_age + report.evicted_by_size + report.payloads_cleared > 0
        || report.vacuumed_pages > 0
    {
        // Fold the deletes into the main file and shrink the WAL. Fails
        // harmlessly (busy) when a reader holds an old snapshot.
        let _checkpoint: rusqlite::Result<(i64, i64, i64)> =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            });
    }

    report.used_bytes_after = page_stats(conn)?.used_bytes();
    report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}
