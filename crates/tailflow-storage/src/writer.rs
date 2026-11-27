//! The single writer thread.
//!
//! One OS thread owns the read-write connection. Ingestion hands it events
//! through a bounded queue and never waits on it, so a slow disk can lose
//! *persisted* history (counted, and recorded as a gap) but can never stall
//! the live view.
//!
//! Durability contract: an event is **persisted** when the transaction that
//! inserted it has committed. Only then is `events_persisted` incremented. With
//! `synchronous = NORMAL` in WAL mode a committed transaction survives a
//! process crash (including `kill -9`); the most recent commits may be lost on
//! OS crash or power loss. `synchronous = FULL` closes that window.

use crate::error::{Result, StorageError};
use crate::health::{Health, LossEntry, LossReason, PendingLoss, WriterState};
use crate::retention::{self, immediate, RetentionPolicy, RetentionReport};
use crate::schema::meta_add;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tailflow_core::event::{ObservabilityEvent, Provenance};

/// An interrupted statement (the progress handler returned `true`); the
/// error conversion reports it as a time-budget overrun.
fn is_interrupt(e: &StorageError) -> bool {
    matches!(e, StorageError::QueryTimeout)
}

pub(crate) enum Command {
    Event(Box<ObservabilityEvent>),
    /// Ack once everything enqueued before this command is committed (or
    /// accounted as lost).
    Flush(SyncSender<()>),
    RetentionNow(SyncSender<Result<RetentionReport>>),
    Shutdown(SyncSender<()>),
}

/// State shared between the handle and the writer.
#[derive(Default)]
pub(crate) struct Shared {
    pub health: Arc<Health>,
    pub pending: Mutex<PendingLoss>,
    /// Losses taken out of `pending` by a commit that has not finished yet.
    /// Counted as pending so a query during a slow retry cannot see neither.
    pub inflight_loss: AtomicU64,
    /// Set when shutdown begins; later submissions are counted as loss.
    pub stopping: AtomicBool,
    pub last_retention: Mutex<Option<(DateTime<Utc>, RetentionReport)>>,
    /// Result of the background integrity check after an unclean exit.
    pub integrity: Mutex<Option<(DateTime<Utc>, Vec<String>)>>,
    /// Called on the writer thread after every commit that inserted rows
    /// (see `Storage::on_commit`).
    pub commit_hooks: Mutex<Vec<CommitHook>>,
}

/// A callback run after a commit. Must be cheap and must not block.
pub(crate) type CommitHook = Arc<dyn Fn(&CommitInfo) + Send + Sync>;

/// What one commit inserted (duplicates excluded).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct CommitInfo {
    pub inserted: u64,
    /// Events other than runtime log lines (`runtime.*`): agent, file,
    /// command, git and system events, including loss markers.
    pub inserted_non_runtime: u64,
    /// `agent.session.started` / `agent.session.ended` events (they trigger
    /// Git snapshots of observed repositories).
    pub inserted_session_boundaries: u64,
}

impl Shared {
    pub fn pending_total(&self) -> u64 {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .total()
            .saturating_add(self.inflight_loss.load(Ordering::SeqCst))
    }

    pub fn add_loss(
        &self,
        reason: LossReason,
        count: u64,
        first: DateTime<Utc>,
        last: DateTime<Utc>,
    ) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .add(reason, count, first, last);
    }
}

pub(crate) struct WriterConfig {
    pub batch_max: usize,
    pub batch_delay: Duration,
    pub fts: bool,
    pub policy: RetentionPolicy,
    pub cleanup_interval: Duration,
    pub heartbeat_interval: Duration,
    /// Wall-clock budget for one retention pass between batches.
    pub retention_budget: Duration,
    /// Storage queue capacity: maintenance yields above a fraction of it.
    pub queue_capacity: usize,
}

/// Queue depth (fraction of capacity) above which maintenance yields to
/// ingestion: a quarter keeps three quarters of the queue as headroom for
/// a burst that arrives while a chunk or merge step is running.
const YIELD_DEPTH_DEN: i64 = 4;
/// Queue depth above which a running full-text compaction is interrupted.
const INTERRUPT_DEPTH_DEN: i64 = 2;
/// Maintenance deferred for longer than this runs regardless of pressure:
/// under sustained load the size cap and the index must still be enforced.
/// A forced step is bounded like any other (chunks, a 64-page merge).
pub(crate) const MAX_MAINTENANCE_DEFERRAL: Duration = Duration::from_secs(60);
/// How soon an owed compaction is retried after being deferred.
const COMPACTION_RETRY: Duration = Duration::from_secs(5);

/// Backoff between commit attempts for one batch. After the last, the batch
/// is counted as lost and recorded as a gap.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(250)];

pub(crate) struct Writer {
    pub conn: Connection,
    pub rx: Receiver<Command>,
    pub shared: Arc<Shared>,
    pub cfg: WriterConfig,
    pub run_id: i64,
    pub next_cleanup: Instant,
    pub next_heartbeat: Instant,
    pub resuming_cleanup: bool,
    /// The cleanup cycle in progress (passes chained by `more_work`).
    pub cycle: retention::Cycle,
    /// Query connections; their idle caches are released on heartbeat ticks.
    pub readers: Arc<crate::ReaderPool>,
    /// A completed cleanup cycle's full-text compaction has not run yet.
    pub fts_owed: bool,
    /// Since when maintenance (deletion or compaction) has been yielding.
    pub deferred_since: Option<Instant>,
}

struct Outcome {
    inserted: u64,
    /// Inserted events that are not runtime log lines (agent, file,
    /// command, git, system). Commit hooks use it to ignore log-only commits.
    inserted_non_runtime: u64,
    inserted_session_boundaries: u64,
    duplicates: u64,
}

impl Writer {
    pub fn run(mut self) {
        self.shared.health.set_state(WriterState::Ok);
        let mut batch: Vec<ObservabilityEvent> = Vec::with_capacity(self.cfg.batch_max);
        loop {
            let wait = self
                .next_heartbeat
                .min(self.next_cleanup)
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            let mut control = None;
            match self.rx.recv_timeout(wait) {
                Ok(Command::Event(e)) => {
                    self.received_one();
                    batch.push(*e);
                    control = self.gather(&mut batch);
                }
                Ok(other) => control = Some(other),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // Every handle was dropped without an explicit shutdown:
                    // treat it as one.
                    self.finish(&mut batch);
                    return;
                }
            }

            if !batch.is_empty() || self.shared.pending_total() > 0 {
                self.commit(&mut batch);
            }

            match control {
                Some(Command::Flush(ack)) => {
                    let _receiver_gone = ack.send(());
                }
                Some(Command::RetentionNow(ack)) => {
                    let report = self.retention();
                    let _receiver_gone = ack.send(report);
                }
                Some(Command::Shutdown(ack)) => {
                    self.finish(&mut batch);
                    let _receiver_gone = ack.send(());
                    return;
                }
                Some(Command::Event(_)) | None => {}
            }

            let now = Instant::now();
            if now >= self.next_heartbeat {
                if let Err(e) = self.heartbeat() {
                    self.note_error(&e);
                }
                // Query connections unused for a whole interval give back
                // their page cache. (Releasing the writer's own cache was
                // measured too: the allocator kept the pages, so no gain.)
                self.readers.release_idle();
                self.next_heartbeat = now + self.cfg.heartbeat_interval;
            }
            if now >= self.next_cleanup {
                let more = match self.retention() {
                    Ok(r) => r.more_work,
                    Err(e) => {
                        self.note_error(&e);
                        false
                    }
                };
                // Liveness probes spawn a process per open run on macOS, so
                // they run on the full interval only, not on resumed passes.
                if !self.resuming_cleanup {
                    if let Err(e) = crate::recover_dead_runs(&self.conn, Some(self.run_id)) {
                        self.note_error(&e);
                    }
                }
                self.resuming_cleanup = more;
                // Unfinished work resumes after the next batch, not after a
                // full interval, so cleanup makes progress without starving
                // ingestion.
                self.next_cleanup = Instant::now()
                    + if more {
                        Duration::from_millis(50)
                    } else if self.fts_owed {
                        COMPACTION_RETRY
                    } else {
                        self.cfg.cleanup_interval
                    };
            }
        }
    }

    fn received_one(&self) {
        self.shared
            .health
            .queue_depth
            .fetch_sub(1, Ordering::SeqCst);
    }

    /// Collect more events until the batch is full or the batch delay since
    /// the first event elapses. Returns a control command that interrupted it.
    fn gather(&mut self, batch: &mut Vec<ObservabilityEvent>) -> Option<Command> {
        let deadline = Instant::now() + self.cfg.batch_delay;
        while batch.len() < self.cfg.batch_max {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Command::Event(e)) => {
                    self.received_one();
                    batch.push(*e);
                }
                Ok(other) => return Some(other),
                Err(_) => break,
            }
        }
        None
    }

    fn note_error(&self, e: &StorageError) {
        let h = &self.shared.health;
        h.storage_errors.fetch_add(1, Ordering::Relaxed);
        h.set_error(e.to_string());
        h.set_state(WriterState::Degraded);
        tracing::warn!(err = %e, "storage write failed");
    }

    /// Commit `batch` plus any pending loss markers, retrying transient
    /// failures. On final failure the batch is counted as lost and becomes a
    /// pending gap, so the loss is recorded once storage recovers.
    fn commit(&mut self, batch: &mut Vec<ObservabilityEvent>) {
        let losses = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let inflight = losses
            .iter()
            .fold(0u64, |acc, (_, e)| acc.saturating_add(e.count));
        self.shared.inflight_loss.store(inflight, Ordering::SeqCst);
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            match self.write(batch, &losses) {
                Ok(outcome) => {
                    let h = &self.shared.health;
                    h.persisted.fetch_add(outcome.inserted, Ordering::Relaxed);
                    h.duplicates
                        .fetch_add(outcome.duplicates, Ordering::Relaxed);
                    h.commits.fetch_add(1, Ordering::Relaxed);
                    h.last_commit_at_us
                        .store(Utc::now().timestamp_micros(), Ordering::Relaxed);
                    h.last_commit_latency_us.store(
                        i64::try_from(started.elapsed().as_micros()).unwrap_or(i64::MAX),
                        Ordering::Relaxed,
                    );
                    h.set_state(WriterState::Ok);
                    self.shared.inflight_loss.store(0, Ordering::SeqCst);
                    batch.clear();
                    if outcome.inserted > 0 {
                        // Clone the list so no hook runs under the lock.
                        let hooks = self
                            .shared
                            .commit_hooks
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        let info = CommitInfo {
                            inserted: outcome.inserted,
                            inserted_non_runtime: outcome.inserted_non_runtime,
                            inserted_session_boundaries: outcome.inserted_session_boundaries,
                        };
                        for hook in hooks {
                            hook(&info);
                        }
                    }
                    return;
                }
                Err(e) => {
                    self.note_error(&e);
                    if let Some(delay) = RETRY_BACKOFF.get(attempt) {
                        attempt += 1;
                        std::thread::sleep(*delay);
                        continue;
                    }
                    let n = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                    let now = Utc::now();
                    let first = batch.iter().map(|e| e.observed_at).min().unwrap_or(now);
                    let last = batch.iter().map(|e| e.observed_at).max().unwrap_or(now);
                    let mut pending = self
                        .shared
                        .pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    pending.restore(losses);
                    pending.add(LossReason::StorageError, n, first, last);
                    self.shared.inflight_loss.store(0, Ordering::SeqCst);
                    drop(pending);
                    self.shared
                        .health
                        .dropped_storage_error
                        .fetch_add(n, Ordering::Relaxed);
                    batch.clear();
                    return;
                }
            }
        }
    }

    fn write(
        &mut self,
        batch: &[ObservabilityEvent],
        losses: &[(LossReason, LossEntry)],
    ) -> Result<Outcome> {
        let run_id = self.run_id;
        let fts = self.cfg.fts;
        immediate(&self.conn, |c| {
            // The newest committed row, by *any* writer: concurrent daemons
            // share the file, and this read happens under the write lock.
            let (mut last_seq, mut floor) = newest(c)?;
            let mut inserted = 0u64;
            let mut inserted_non_runtime = 0u64;
            let mut inserted_session_boundaries = 0u64;
            let mut duplicates = 0u64;
            for (reason, loss) in losses {
                record_gap(
                    c,
                    *reason,
                    Some(loss.count),
                    loss.first_at,
                    loss.last_at,
                    last_seq,
                    Some(run_id),
                    None,
                )?;
                let marker = loss_marker(*reason, Some(loss.count), loss.first_at, loss.last_at);
                if let Some(seq) = insert_event(c, &marker, &mut floor, run_id, fts)? {
                    last_seq = seq;
                    inserted += 1;
                    inserted_non_runtime += 1;
                }
            }
            for event in batch {
                match insert_event(c, event, &mut floor, run_id, fts)? {
                    Some(seq) => {
                        last_seq = seq;
                        inserted += 1;
                        if !event.event_type.starts_with("runtime.") {
                            inserted_non_runtime += 1;
                        }
                        if matches!(
                            event.event_type.as_str(),
                            "agent.session.started" | "agent.session.ended"
                        ) {
                            inserted_session_boundaries += 1;
                        }
                    }
                    None => duplicates += 1,
                }
            }
            meta_add(
                c,
                "event_count",
                i64::try_from(inserted).unwrap_or(i64::MAX),
            )?;
            c.execute(
                // `ended_at = NULL` re-arms a run another daemon wrongly declared
                // dead (e.g. after laptop sleep) so a later crash is detected.
                "UPDATE runs SET heartbeat_at = ?1, last_seq = max(last_seq, ?2), ended_at = NULL
                 WHERE id = ?3",
                params![Utc::now().timestamp_micros(), last_seq, run_id],
            )?;
            Ok(Outcome {
                inserted,
                inserted_non_runtime,
                inserted_session_boundaries,
                duplicates,
            })
        })
    }

    fn heartbeat(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET heartbeat_at = ?1, ended_at = NULL WHERE id = ?2",
            params![Utc::now().timestamp_micros(), self.run_id],
        )?;
        Ok(())
    }

    /// Whether maintenance must run now even under pressure.
    fn maintenance_forced(&self) -> bool {
        self.deferred_since
            .is_some_and(|t| t.elapsed() >= MAX_MAINTENANCE_DEFERRAL)
    }

    fn queue_deep(&self, den: i64) -> bool {
        let cap = i64::try_from(self.cfg.queue_capacity).unwrap_or(i64::MAX);
        self.shared.health.queue_depth.load(Ordering::Relaxed) > cap / den
    }

    /// Run an owed full-text compaction unless ingestion is under pressure.
    /// A compaction that starts is interrupted (and rolled back, to be
    /// retried) if the queue fills past half while it runs — unless it was
    /// already deferred too long.
    fn maybe_compact(&mut self, report: &mut RetentionReport) {
        if !self.fts_owed {
            return;
        }
        let forced = self.maintenance_forced();
        let h = &self.shared.health;
        if !forced && self.queue_deep(YIELD_DEPTH_DEN) {
            h.fts_compaction_deferred.fetch_add(1, Ordering::Relaxed);
            self.deferred_since.get_or_insert_with(Instant::now);
            return;
        }
        if forced {
            h.fts_compaction_forced.fetch_add(1, Ordering::Relaxed);
        } else {
            let health = self.shared.health.clone();
            let cap = i64::try_from(self.cfg.queue_capacity).unwrap_or(i64::MAX);
            let installed = self.conn.progress_handler(
                10_000,
                Some(move || {
                    health.queue_depth.load(Ordering::Relaxed) > cap / INTERRUPT_DEPTH_DEN
                }),
            );
            if let Err(e) = installed {
                // Without the handler the step cannot be interrupted; defer it.
                tracing::warn!(err = %e, "could not arm compaction interrupt; deferring");
                h.fts_compaction_deferred.fetch_add(1, Ordering::Relaxed);
                self.deferred_since.get_or_insert_with(Instant::now);
                return;
            }
        }
        let result = retention::compact_fts(&self.conn);
        if !forced {
            if let Err(e) = self.conn.progress_handler(0, None::<fn() -> bool>) {
                tracing::warn!(err = %e, "could not remove the compaction interrupt");
            }
        }
        match result {
            Ok(()) => {
                report.fts_compacted = true;
                self.fts_owed = false;
                self.deferred_since = None;
            }
            Err(e) if is_interrupt(&e) => {
                h.fts_compaction_deferred.fetch_add(1, Ordering::Relaxed);
                self.deferred_since.get_or_insert_with(Instant::now);
            }
            Err(e) => {
                // BUSY from a concurrent CLI command, for example: retry with
                // the next cycle, never fail the pass.
                tracing::warn!(err = %e, "full-text compaction skipped");
            }
        }
    }

    fn retention(&mut self) -> Result<RetentionReport> {
        let forced = self.maintenance_forced();
        let health = self.shared.health.clone();
        let cap = i64::try_from(self.cfg.queue_capacity).unwrap_or(i64::MAX);
        let under_pressure =
            move || !forced && health.queue_depth.load(Ordering::Relaxed) > cap / YIELD_DEPTH_DEN;
        let result = retention::run_with_pressure(
            &self.conn,
            &self.cfg.policy,
            Utc::now().timestamp_micros(),
            self.cfg.retention_budget,
            self.cycle,
            &under_pressure,
        );
        let mut report = match result {
            Ok(r) => r,
            Err(e) => {
                // The cycle is abandoned; the next one starts afresh.
                self.cycle = retention::Cycle::default();
                return Err(e);
            }
        };
        if report.yielded {
            self.shared
                .health
                .maintenance_yields
                .fetch_add(1, Ordering::Relaxed);
            self.deferred_since.get_or_insert_with(Instant::now);
        } else if !self.fts_owed && !report.more_work {
            // Only a *finished* cycle ends the deferral: a forced pass that
            // ran out of time keeps the cycle forced until it completes, so
            // sustained pressure cannot hold cleanup to one slice per minute.
            self.deferred_since = None;
        }
        if report.fts_compaction_due {
            self.fts_owed = true;
        }
        self.maybe_compact(&mut report);
        self.cycle = self.cycle.after(
            report.expired_by_age + report.evicted_by_size > 0,
            report.more_work,
        );
        if report.expired_by_age + report.evicted_by_size > 0 {
            tracing::info!(
                expired = report.expired_by_age,
                evicted = report.evicted_by_size,
                ms = report.duration_ms,
                "storage retention"
            );
        }
        *self
            .shared
            .last_retention
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some((Utc::now(), report.clone()));
        Ok(report)
    }

    /// Drain the queue, commit everything, and mark the run clean.
    fn finish(&mut self, batch: &mut Vec<ObservabilityEvent>) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let mut acks = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Command::Event(e)) => {
                    self.received_one();
                    batch.push(*e);
                    if batch.len() >= self.cfg.batch_max {
                        self.commit(batch);
                    }
                }
                Ok(Command::Flush(ack) | Command::Shutdown(ack)) => acks.push(ack),
                Ok(Command::RetentionNow(ack)) => {
                    let _receiver_gone = ack.send(Err(StorageError::WriterStopped));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if !batch.is_empty() || self.shared.pending_total() > 0 {
            self.commit(batch);
        }
        // Anything still counted in the queue raced the final drain and is
        // lost with the receiver; unrecorded pending loss is lost too. Either
        // way the run is left *open* (ended_at NULL) so the next start
        // records an unclean-shutdown gap instead of trusting this run.
        let in_queue = self.shared.health.queue_depth.load(Ordering::SeqCst);
        let clean = self.shared.pending_total() == 0 && in_queue <= 0;
        let ended = if clean {
            self.conn.execute(
                "UPDATE runs SET ended_at = ?1, heartbeat_at = ?1, clean = 1 WHERE id = ?2",
                params![Utc::now().timestamp_micros(), self.run_id],
            )
        } else {
            tracing::warn!(
                in_queue,
                pending = self.shared.pending_total(),
                "storage stopped with unrecorded loss; the next start will record a gap"
            );
            self.conn.execute(
                "UPDATE runs SET ended_at = NULL, clean = 0 WHERE id = ?1",
                params![self.run_id],
            )
        };
        if let Err(e) = ended {
            self.note_error(&e.into());
        }
        // Refresh planner statistics; cheap when nothing changed.
        let _optimize_best_effort = self.conn.execute_batch("PRAGMA optimize");
        let _checkpoint_best_effort: rusqlite::Result<(i64, i64, i64)> =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                });
        self.shared.health.set_state(WriterState::Stopped);
        for ack in acks {
            let _receiver_gone = ack.send(());
        }
    }
}

/// `(highest seq ever issued, observed_at floor)`.
///
/// The seq comes from `sqlite_sequence`, not `max(seq)`: after retention or a
/// purge empties the table, a gap must still be anchored where the lost
/// events would have been, not at 0 (which would place it outside every
/// later query's range and make that range look complete).
pub(crate) fn newest(c: &Connection) -> Result<(i64, i64)> {
    let issued: i64 = c
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'events'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let floor: i64 = c
        .query_row(
            "SELECT observed_at FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(i64::MIN);
    Ok((issued, floor))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_gap(
    c: &Connection,
    reason: LossReason,
    count: Option<u64>,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    at_seq: i64,
    run_id: Option<i64>,
    detail: Option<&str>,
) -> Result<()> {
    record_gap_range(
        c, reason, count, first, last, at_seq, at_seq, run_id, detail,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_gap_range(
    c: &Connection,
    reason: LossReason,
    count: Option<u64>,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    from_seq: i64,
    to_seq: i64,
    run_id: Option<i64>,
    detail: Option<&str>,
) -> Result<()> {
    c.execute(
        "INSERT INTO gaps (reason, count, first_at, last_at, from_seq, to_seq, run_id, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            reason.as_str(),
            count.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            first.timestamp_micros(),
            last.timestamp_micros(),
            from_seq,
            to_seq,
            run_id,
            detail,
        ],
    )?;
    Ok(())
}

/// The `system.events.dropped` event that makes a loss visible in the event
/// stream itself, not only in the gaps table.
pub(crate) fn loss_marker(
    reason: LossReason,
    count: Option<u64>,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
) -> ObservabilityEvent {
    let mut e = ObservabilityEvent::new(
        format!("loss:{}:{}", reason.as_str(), Utc::now().timestamp_micros()),
        "system.events.dropped",
        Provenance::SystemObserved,
        "daemon",
        "tailflow",
        last,
    );
    e.severity = tailflow_core::LogLevel::Warn;
    e.message = Some(match count {
        Some(n) => format!("{n} events were not persisted ({})", reason.as_str()),
        None => format!(
            "an unknown number of events may be missing ({})",
            reason.as_str()
        ),
    });
    let mut attrs = Map::new();
    attrs.insert("reason".into(), Value::from(reason.as_str()));
    attrs.insert("count".into(), count.map_or(Value::Null, Value::from));
    attrs.insert("first_at".into(), Value::from(first.to_rfc3339()));
    attrs.insert("last_at".into(), Value::from(last.to_rfc3339()));
    e.attributes = attrs;
    e
}

/// Insert one event. Returns its seq, or `None` when it was a duplicate of a
/// stable `event_id` already stored.
///
/// `floor` enforces the ordering invariant: stored `observed_at` never
/// decreases in seq order (an event may be raised to the previous row's
/// value), and stored `ts` never exceeds stored `observed_at` (a future-dated
/// source timestamp is clamped and the original kept as
/// `attributes.source_timestamp`). Together these make `since` translate into
/// an exact seq lower bound.
pub(crate) fn insert_event(
    c: &Connection,
    e: &ObservabilityEvent,
    floor: &mut i64,
    run_id: i64,
    fts: bool,
) -> Result<Option<i64>> {
    let observed = e.observed_at.timestamp_micros().max(*floor);
    let mut ts = e.timestamp.timestamp_micros();
    let mut attributes = std::borrow::Cow::Borrowed(&e.attributes);
    if ts > observed {
        ts = observed;
        attributes.to_mut().insert(
            "source_timestamp".into(),
            Value::from(e.timestamp.to_rfc3339()),
        );
    }
    let attributes_json = if attributes.is_empty() {
        None
    } else {
        Some(serde_json::to_string(attributes.as_ref()).map_err(|err| {
            StorageError::InvalidQuery(format!("attributes not serializable: {err}"))
        })?)
    };
    let redaction_json = if e.redaction.is_empty() {
        None
    } else {
        serde_json::to_string(&e.redaction).ok()
    };
    let mut stmt = c.prepare_cached(
        "INSERT INTO events (
            event_id, dedup_key, schema_version, ts, observed_at, event_type, provenance,
            source_type, source_id, project_id, session_id, operation_id, parent_operation_id,
            agent_name, severity, status, duration_ms, path, message, attributes, payload,
            redaction, run_id
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)
         ON CONFLICT(dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING",
    )?;
    let changed = stmt.execute(params![
        e.event_id,
        e.stable_id.then_some(e.event_id.as_str()),
        i64::from(e.schema_version),
        ts,
        observed,
        e.event_type,
        e.provenance.as_str(),
        e.source_type,
        e.source_id,
        e.project_id,
        e.session_id,
        e.operation_id,
        e.parent_operation_id,
        e.agent_name,
        i64::from(e.severity.severity()),
        e.status.map(|s| s.as_str()),
        e.duration_ms.map(|d| i64::try_from(d).unwrap_or(i64::MAX)),
        e.path,
        e.message,
        attributes_json,
        e.payload,
        redaction_json,
        run_id,
    ])?;
    if changed == 0 {
        return Ok(None);
    }
    let seq = c.last_insert_rowid();
    *floor = observed;
    crate::sessions::upsert(c, e, seq, ts)?;
    if fts {
        if let Some(message) = &e.message {
            c.prepare_cached("INSERT INTO events_fts(rowid, message) VALUES (?1, ?2)")?
                .execute(params![seq, message])?;
        }
    }
    Ok(Some(seq))
}
