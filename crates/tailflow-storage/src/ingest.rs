//! Moving agent events from the spool into storage.
//!
//! One pass ([`SpoolIngestor::drain_once`]):
//! 1. Return stale claims of dead owners to the inbox (replay is safe:
//!    every spooled event has a stable id that storage deduplicates).
//! 2. Turn the hook-side overflow counter into a recorded gap.
//! 3. Retry this owner's unfinished claims, then claim new records by
//!    rename, oldest first, up to a batch.
//! 4. Submit each event with backpressure (a full queue makes the
//!    ingestor wait, not drop), then **flush**: a record file is deleted
//!    only after storage confirms the commit, so a crash at any point
//!    leaves the record to be replayed rather than lost.
//!
//! Events get `observed_at = now` (when storage received them), keeping
//! the store's non-decreasing `observed_at` invariant; `timestamp` stays the
//! time the agent reported the activity. Records older than a day are
//! still ingested and their events marked `attributes.late_delivery`.

use crate::health::LossReason;
use crate::{Storage, Submitted};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tailflow_core::agent::spool::{RejectReason, Spool, SpoolError, LATE_AFTER};

/// Records claimed per pass.
pub const BATCH_FILES: usize = 256;
/// A claim untouched this long belongs to a dead daemon.
pub const STALE_CLAIM: Duration = Duration::from_secs(300);
/// How long one event may wait for queue space.
const SUBMIT_WAIT: Duration = Duration::from_secs(10);
const FLUSH_TIMEOUT: Duration = Duration::from_secs(30);
/// Temp files older than this are leftovers of a hook that died mid-write.
const STALE_TEMP: Duration = Duration::from_secs(3600);
/// Attempts before a record whose events keep being dropped is given up
/// (its loss is already counted and recorded by storage).
const MAX_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestReport {
    pub files: usize,
    pub events_submitted: usize,
    pub events_dropped: usize,
    pub excluded: u64,
    pub rejected: u64,
    pub corrupt_files: usize,
    pub overflow: u64,
    pub recovered_claims: usize,
    /// More records were waiting than one batch.
    pub more: bool,
    pub errors: Vec<String>,
}

/// Drains one spool into one storage, as one claim owner.
pub struct SpoolIngestor {
    spool: Spool,
    owner: String,
    attempts: Mutex<HashMap<PathBuf, u8>>,
    stop: Arc<AtomicBool>,
}

impl SpoolIngestor {
    pub fn new(spool: Spool, owner: impl Into<String>) -> Self {
        Self {
            spool,
            owner: owner.into(),
            attempts: Mutex::new(HashMap::new()),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn spool(&self) -> &Spool {
        &self.spool
    }

    /// A flag that makes a pass in progress stop after the current event
    /// (shutdown). Unfinished records stay claimed and are released or
    /// replayed later; their events are deduplicated.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Note a failed attempt on `path`; true when it should be given up.
    fn give_up(&self, path: &Path) -> bool {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let n = attempts.entry(path.to_path_buf()).or_insert(0);
        *n = n.saturating_add(1);
        if *n >= MAX_ATTEMPTS {
            attempts.remove(path);
            true
        } else {
            false
        }
    }

    /// One bounded pass. Blocking (waits for queue space and a flush).
    pub fn drain_once(&self, storage: &Storage) -> IngestReport {
        let mut r = IngestReport::default();
        match self.spool.recover_stale_claims(&self.owner, STALE_CLAIM) {
            Ok(n) => r.recovered_claims = n,
            Err(e) => r.errors.push(format!("recovering claims: {e}")),
        }
        if let Err(e) = self.spool.clean_temp_files(STALE_TEMP) {
            r.errors.push(format!("cleaning temp files: {e}"));
        }
        // Read the hook-side overflow counter now, but consume it only after
        // the gap that records it has been committed (below).
        let overflow = match self.spool.peek_overflow() {
            Ok(o) => o,
            Err(e) => {
                r.errors.push(format!("reading overflow: {e}"));
                None
            }
        };
        if let Some(o) = &overflow {
            let now = Utc::now();
            storage.record_agent_loss(
                LossReason::AgentSpoolOverflow,
                o.count,
                o.first_at.unwrap_or(now),
                o.last_at.unwrap_or(now),
            );
            r.overflow = o.count;
        }

        let mut claimed: Vec<PathBuf> = match self.spool.own_claims(&self.owner) {
            Ok(v) => v,
            Err(e) => {
                r.errors.push(format!("listing own claims: {e}"));
                Vec::new()
            }
        };
        let room = BATCH_FILES.saturating_sub(claimed.len());
        let mut newly_claimed = 0usize;
        match self.spool.pending(room.saturating_add(1)) {
            Ok(pending) => {
                r.more = pending.len() > room;
                for p in pending.into_iter().take(room) {
                    match self.spool.claim(&p, &self.owner) {
                        Ok(Some(c)) => {
                            claimed.push(c);
                            newly_claimed += 1;
                        }
                        Ok(None) => {} // another daemon took it
                        Err(e) => r.errors.push(format!("claiming: {e}")),
                    }
                }
            }
            Err(e) => r.errors.push(format!("listing spool: {e}")),
        }
        if newly_claimed == 0 {
            // Nothing could be claimed this pass (all taken, or claiming
            // fails): do not report `more`, so the caller backs off instead
            // of spinning on a directory it cannot make progress on.
            r.more = false;
        }

        let now = Utc::now();
        let mut done: Vec<PathBuf> = Vec::new();
        for path in claimed {
            if self.stopping() {
                break;
            }
            r.files += 1;
            let mut rec = match Spool::read(&path) {
                Ok(rec) => rec,
                Err(SpoolError::Corrupt(m)) => {
                    // Count unknown; at least one hook call is lost.
                    storage.record_agent_loss(LossReason::AgentSpoolCorrupt, 1, now, now);
                    r.corrupt_files += 1;
                    r.errors.push(format!("corrupt spool record dropped: {m}"));
                    done.push(path);
                    continue;
                }
                Err(SpoolError::Io(e)) => {
                    r.errors.push(format!("reading spool record: {e}"));
                    if self.give_up(&path) {
                        // Persistently unreadable (a directory, permissions):
                        // record it as loss and move it aside, so it cannot
                        // occupy a batch slot forever.
                        storage.record_agent_loss(LossReason::AgentSpoolCorrupt, 1, now, now);
                        r.corrupt_files += 1;
                        if let Err(e) = self.spool.quarantine(&path) {
                            r.errors
                                .push(format!("quarantining unreadable record: {e}"));
                        }
                    }
                    continue;
                }
            };
            // Exclusions and rejections are counted once per record: after
            // counting, the claimed file is rewritten without them, so a
            // replay (retry, crash, stale claim) counts only its events,
            // which storage deduplicates. A crash between the two steps
            // double counts, which over-reports loss rather than hiding it.
            if rec.excluded > 0 || !rec.rejected.is_empty() {
                if rec.excluded > 0 {
                    storage.add_excluded(u64::from(rec.excluded));
                    r.excluded += u64::from(rec.excluded);
                }
                for rej in &rec.rejected {
                    storage.record_agent_loss(
                        LossReason::AgentInputRejected,
                        u64::from(rej.count),
                        rec.written_at,
                        rec.written_at,
                    );
                    r.rejected += u64::from(rej.count);
                    if rej.reason == RejectReason::InvalidConfig {
                        r.errors.push(
                            "agent capture config is invalid; events were not recorded".into(),
                        );
                    }
                }
                rec.excluded = 0;
                rec.rejected.clear();
                if let Err(e) = Spool::rewrite(&path, &rec) {
                    r.errors.push(format!("marking record counted: {e}"));
                }
            }
            let late = now
                .signed_duration_since(rec.written_at)
                .to_std()
                .is_ok_and(|age| age > LATE_AFTER);
            let mut dropped = false;
            for mut e in rec.events {
                if self.stopping() {
                    dropped = true; // unfinished: keep the claim, replay later
                    break;
                }
                e.observed_at = Utc::now();
                if late {
                    e.attributes
                        .insert("late_delivery".into(), serde_json::Value::Bool(true));
                }
                match storage.submit_backpressured(e, Instant::now() + SUBMIT_WAIT) {
                    Submitted::Enqueued => r.events_submitted += 1,
                    Submitted::Excluded | Submitted::Invalid => {}
                    Submitted::Dropped(_) => {
                        r.events_dropped += 1;
                        dropped = true;
                    }
                }
            }
            if dropped && (self.stopping() || !self.give_up(&path)) {
                continue; // keep the claim; replayed next pass (deduplicated)
            }
            done.push(path);
        }
        // Delete only what storage has committed.
        match storage.flush(FLUSH_TIMEOUT) {
            Ok(()) => {
                if let Some(o) = &overflow {
                    if let Err(e) = self.spool.consume_overflow(o.count) {
                        r.errors
                            .push(format!("resetting the overflow counter: {e}"));
                    }
                }
                for p in done {
                    if let Err(e) = Spool::complete(&p) {
                        r.errors.push(format!("removing ingested record: {e}"));
                    }
                }
            }
            Err(e) => r
                .errors
                .push(format!("flush failed; records kept for replay: {e}")),
        }
        if !r.more {
            if let Err(e) = self.spool.refresh_full_marker() {
                r.errors
                    .push(format!("clearing the spool-full marker: {e}"));
            }
        }
        r
    }

    /// Drain until nothing is waiting (or `budget` elapses).
    pub fn drain(&self, storage: &Storage, budget: Duration) -> IngestReport {
        let deadline = Instant::now() + budget;
        let mut total = IngestReport::default();
        loop {
            let r = self.drain_once(storage);
            total.files += r.files;
            total.events_submitted += r.events_submitted;
            total.events_dropped += r.events_dropped;
            total.excluded += r.excluded;
            total.rejected += r.rejected;
            total.corrupt_files += r.corrupt_files;
            total.overflow += r.overflow;
            total.recovered_claims += r.recovered_claims;
            total.errors.extend(r.errors);
            total.more = r.more;
            if !r.more || Instant::now() >= deadline || r.files == 0 || self.stopping() {
                return total;
            }
        }
    }

    /// Hand unfinished claims back to the inbox (on clean shutdown).
    pub fn release(&self) {
        if let Err(e) = self.spool.release_claims(&self.owner) {
            tracing::warn!(err = %e, "could not release agent spool claims");
        }
    }
}
