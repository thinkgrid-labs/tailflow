//! Ingestion and persistence counters.
//!
//! Every counter here is something the code can actually observe. There is
//! deliberately no "events lost" total that pretends to know how many lines a
//! killed process never printed: an unclean shutdown is recorded as a gap of
//! *unknown* size.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

/// Why events were lost. Stored as text in `gaps.reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LossReason {
    /// The live ingestion bus overflowed before records reached TailFlow's
    /// fan-out (affects the live view *and* storage).
    IngestLag,
    /// The storage queue was full; the live view was unaffected.
    QueueFull,
    /// A batch could not be committed after retries.
    StorageError,
    /// Submitted after the writer stopped.
    WriterStopped,
    /// A previous daemon run ended without a clean shutdown. Count unknown.
    UncleanShutdown,
    /// Rows deliberately removed for one session.
    SessionDeleted,
    /// Failed schema validation after sanitizing (e.g. an oversized id).
    InvalidEvent,
}

impl LossReason {
    pub fn as_str(self) -> &'static str {
        match self {
            LossReason::IngestLag => "ingest_lag",
            LossReason::QueueFull => "queue_full",
            LossReason::StorageError => "storage_error",
            LossReason::WriterStopped => "writer_stopped",
            LossReason::UncleanShutdown => "unclean_shutdown",
            LossReason::SessionDeleted => "session_deleted",
            LossReason::InvalidEvent => "invalid_event",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LossEntry {
    pub count: u64,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

/// Losses detected but not yet written as durable gap rows.
#[derive(Debug, Default)]
pub(crate) struct PendingLoss {
    entries: BTreeMap<LossReason, LossEntry>,
}

impl PendingLoss {
    pub fn add(
        &mut self,
        reason: LossReason,
        count: u64,
        first: DateTime<Utc>,
        last: DateTime<Utc>,
    ) {
        if count == 0 {
            return;
        }
        self.entries
            .entry(reason)
            .and_modify(|e| {
                e.count = e.count.saturating_add(count);
                e.first_at = e.first_at.min(first);
                e.last_at = e.last_at.max(last);
            })
            .or_insert(LossEntry {
                count,
                first_at: first,
                last_at: last,
            });
    }

    pub fn take(&mut self) -> Vec<(LossReason, LossEntry)> {
        std::mem::take(&mut self.entries).into_iter().collect()
    }

    pub fn restore(&mut self, items: Vec<(LossReason, LossEntry)>) {
        for (reason, e) in items {
            self.add(reason, e.count, e.first_at, e.last_at);
        }
    }

    pub fn total(&self) -> u64 {
        self.entries
            .values()
            .fold(0u64, |acc, e| acc.saturating_add(e.count))
    }
}

/// Writer state, as last observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterState {
    Starting,
    Ok,
    /// The most recent commit failed; the writer keeps retrying.
    Degraded,
    Stopped,
}

impl WriterState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => WriterState::Ok,
            2 => WriterState::Degraded,
            3 => WriterState::Stopped,
            _ => WriterState::Starting,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            WriterState::Starting => 0,
            WriterState::Ok => 1,
            WriterState::Degraded => 2,
            WriterState::Stopped => 3,
        }
    }
}

#[derive(Debug, Default)]
pub struct Health {
    pub(crate) received: AtomicU64,
    pub(crate) enqueued: AtomicU64,
    pub(crate) persisted: AtomicU64,
    pub(crate) duplicates: AtomicU64,
    pub(crate) excluded: AtomicU64,
    pub(crate) invalid: AtomicU64,
    pub(crate) dropped_queue_full: AtomicU64,
    pub(crate) dropped_storage_error: AtomicU64,
    pub(crate) dropped_writer_stopped: AtomicU64,
    pub(crate) dropped_ingest_lag: AtomicU64,
    pub(crate) storage_errors: AtomicU64,
    pub(crate) commits: AtomicU64,
    pub(crate) queue_depth: AtomicI64,
    pub(crate) last_event_at_us: AtomicI64,
    pub(crate) last_commit_at_us: AtomicI64,
    pub(crate) last_commit_latency_us: AtomicI64,
    state: AtomicU8,
    pub(crate) last_error: Mutex<Option<(DateTime<Utc>, String)>>,
}

impl Health {
    pub(crate) fn set_state(&self, s: WriterState) {
        self.state.store(s.to_u8(), Ordering::SeqCst);
    }

    pub fn state(&self) -> WriterState {
        WriterState::from_u8(self.state.load(Ordering::SeqCst))
    }

    pub(crate) fn set_error(&self, message: String) {
        let mut guard = self.last_error.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some((Utc::now(), message));
    }

    pub fn snapshot(&self, pending_loss: u64) -> HealthSnapshot {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let ts = |a: &AtomicI64| {
            let v = a.load(Ordering::Relaxed);
            (v > 0)
                .then(|| DateTime::<Utc>::from_timestamp_micros(v))
                .flatten()
        };
        let dropped = DroppedCounts {
            ingest_lag: load(&self.dropped_ingest_lag),
            queue_full: load(&self.dropped_queue_full),
            storage_error: load(&self.dropped_storage_error),
            writer_stopped: load(&self.dropped_writer_stopped),
            invalid: load(&self.invalid),
        };
        let last_error = self
            .last_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        HealthSnapshot {
            writer_state: self.state(),
            events_received: load(&self.received),
            events_enqueued: load(&self.enqueued),
            events_persisted: load(&self.persisted),
            duplicates_ignored: load(&self.duplicates),
            events_excluded: load(&self.excluded),
            events_invalid: load(&self.invalid),
            events_dropped_total: dropped.total(),
            events_dropped: dropped,
            loss_detected: dropped.total() > 0,
            pending_loss_unrecorded: pending_loss,
            storage_errors: load(&self.storage_errors),
            commits: load(&self.commits),
            ingestion_queue_depth: u64::try_from(self.queue_depth.load(Ordering::Relaxed).max(0))
                .unwrap_or(0),
            last_event_at: ts(&self.last_event_at_us),
            last_commit_at: ts(&self.last_commit_at_us),
            last_commit_latency_ms: {
                let us = self.last_commit_latency_us.load(Ordering::Relaxed);
                #[allow(clippy::cast_precision_loss)]
                let ms = us as f64 / 1000.0;
                (us > 0).then_some(ms)
            },
            last_error_at: last_error.as_ref().map(|(t, _)| *t),
            last_error: last_error.map(|(_, m)| m),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct DroppedCounts {
    pub ingest_lag: u64,
    pub queue_full: u64,
    pub storage_error: u64,
    pub writer_stopped: u64,
    pub invalid: u64,
}

impl DroppedCounts {
    pub fn total(&self) -> u64 {
        self.ingest_lag
            .saturating_add(self.queue_full)
            .saturating_add(self.storage_error)
            .saturating_add(self.writer_stopped)
            .saturating_add(self.invalid)
    }
}

/// Point-in-time view for status endpoints. Counters cover this daemon run.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub writer_state: WriterState,
    /// Offered to storage by ingestion (before exclusion and queueing).
    pub events_received: u64,
    pub events_enqueued: u64,
    /// Committed to SQLite. An event counts only after its COMMIT returned.
    pub events_persisted: u64,
    pub duplicates_ignored: u64,
    /// Refused by `[privacy]` collection boundaries (deliberate, not loss).
    pub events_excluded: u64,
    /// Rejected by schema validation.
    pub events_invalid: u64,
    pub events_dropped_total: u64,
    pub events_dropped: DroppedCounts,
    /// True once any loss has been detected in this run. Never resets.
    pub loss_detected: bool,
    /// Detected losses not yet written as gap rows (writer behind or failing).
    pub pending_loss_unrecorded: u64,
    pub storage_errors: u64,
    pub commits: u64,
    pub ingestion_queue_depth: u64,
    pub last_event_at: Option<DateTime<Utc>>,
    pub last_commit_at: Option<DateTime<Utc>>,
    pub last_commit_latency_ms: Option<f64>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}
