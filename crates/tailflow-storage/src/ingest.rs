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
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tailflow_core::agent::spool::{RejectReason, Spool, SpoolError, LATE_AFTER};

/// Records claimed per pass.
pub const BATCH_FILES: usize = 256;
/// A claim untouched this long belongs to a dead daemon.
pub const STALE_CLAIM: Duration = Duration::from_secs(300);
/// How long one event may wait for queue space.
const SUBMIT_WAIT: Duration = Duration::from_secs(10);
const FLUSH_TIMEOUT: Duration = Duration::from_secs(30);
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
}

impl SpoolIngestor {
    pub fn new(spool: Spool, owner: impl Into<String>) -> Self {
        Self {
            spool,
            owner: owner.into(),
            attempts: Mutex::new(HashMap::new()),
        }
    }

    pub fn spool(&self) -> &Spool {
        &self.spool
    }

    /// One bounded pass. Blocking (waits for queue space and a flush).
    pub fn drain_once(&self, storage: &Storage) -> IngestReport {
        let mut r = IngestReport::default();
        match self.spool.recover_stale_claims(&self.owner, STALE_CLAIM) {
            Ok(n) => r.recovered_claims = n,
            Err(e) => r.errors.push(format!("recovering claims: {e}")),
        }
        match self.spool.take_overflow() {
            Ok(Some(o)) => {
                let now = Utc::now();
                storage.record_agent_loss(
                    LossReason::AgentSpoolOverflow,
                    o.count,
                    o.first_at.unwrap_or(now),
                    o.last_at.unwrap_or(now),
                );
                r.overflow = o.count;
            }
            Ok(None) => {}
            Err(e) => r.errors.push(format!("reading overflow: {e}")),
        }

        let mut claimed: Vec<PathBuf> = match self.spool.own_claims(&self.owner) {
            Ok(v) => v,
            Err(e) => {
                r.errors.push(format!("listing own claims: {e}"));
                Vec::new()
            }
        };
        let room = BATCH_FILES.saturating_sub(claimed.len());
        match self.spool.pending(room.saturating_add(1)) {
            Ok(pending) => {
                r.more = pending.len() > room;
                for p in pending.into_iter().take(room) {
                    match self.spool.claim(&p, &self.owner) {
                        Ok(Some(c)) => claimed.push(c),
                        Ok(None) => {} // another daemon took it
                        Err(e) => r.errors.push(format!("claiming: {e}")),
                    }
                }
            }
            Err(e) => r.errors.push(format!("listing spool: {e}")),
        }
        if claimed.is_empty() {
            if let Err(e) = self.spool.refresh_full_marker() {
                r.errors
                    .push(format!("clearing the spool-full marker: {e}"));
            }
            return r;
        }

        let now = Utc::now();
        let mut done: Vec<PathBuf> = Vec::new();
        for path in claimed {
            r.files += 1;
            let rec = match Spool::read(&path) {
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
                    continue;
                }
            };
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
                    r.errors
                        .push("agent capture config is invalid; events were not recorded".into());
                }
            }
            let late = now
                .signed_duration_since(rec.written_at)
                .to_std()
                .is_ok_and(|age| age > LATE_AFTER);
            let mut dropped = false;
            for mut e in rec.events {
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
            if dropped {
                let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
                let n = attempts.entry(path.clone()).or_insert(0);
                *n = n.saturating_add(1);
                if *n < MAX_ATTEMPTS {
                    continue; // keep the claim; replayed next pass (deduplicated)
                }
                attempts.remove(&path);
            }
            done.push(path);
        }
        // Delete only what storage has committed.
        match storage.flush(FLUSH_TIMEOUT) {
            Ok(()) => {
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
            if !r.more || Instant::now() >= deadline || r.files == 0 {
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
