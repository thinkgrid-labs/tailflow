//! Durable, local, single-writer SQLite event store for TailFlow.
//!
//! ```text
//! ingestion ──try_send──► bounded queue ──► writer thread ──► SQLite (WAL)
//!     │ (never blocks)        │ full → counted + gap row          ▲
//!     ▼                                                           │
//!  live ring / SSE                               reader pool ─────┘ /api/v1/*
//! ```
//!
//! See `writer` for the durability contract, `retention` for expiry rules,
//! and `query` for ordering and pagination.

#![forbid(unsafe_code)]

pub mod error;
pub mod evidence;
pub mod health;
pub mod ingest;
pub mod paths;
pub mod query;
pub mod retention;
pub mod schema;
pub mod sessions;
pub mod stats;
mod writer;

pub use error::{Result, StorageError};
pub use health::{HealthSnapshot, LossReason, WriterState};
pub use query::{EventPage, EventQuery, Order, StoredEvent};
pub use retention::{RetentionPolicy, RetentionReport};
pub use writer::CommitInfo;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tailflow_core::config::StorageConfig;
use tailflow_core::event::ObservabilityEvent;
use tailflow_core::privacy::CapturePolicy;
use writer::{Command, Shared, Writer, WriterConfig};

/// Outcome of [`Storage::coverage_started`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageClaim {
    /// This run observes the root; the coverage row id.
    Claimed(i64),
    /// Another live daemon run already observes it.
    HeldBy { run_id: i64, pid: i64 },
}

/// A run whose heartbeat is older than this is considered dead even if a
/// process with its pid exists (pid reuse).
const STALE_RUN: Duration = Duration::from_secs(60);
const HEARTBEAT: Duration = Duration::from_secs(15);
const READERS: usize = 4;

/// Options that are not part of the user's `[storage]` config.
pub struct OpenOptions {
    pub project_id: Option<String>,
    /// Recorded on the run row.
    pub version: String,
    pub policy: Arc<CapturePolicy>,
}

/// What happened to one submitted event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Submitted {
    Enqueued,
    /// Refused by a `[privacy]` collection boundary. Deliberate, not loss.
    Excluded,
    /// Failed schema validation. Counted, not stored.
    Invalid,
    /// Lost; counted and recorded as a gap.
    Dropped(LossReason),
}

/// A previous run that did not shut down cleanly, found at open.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredRun {
    pub run_id: i64,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub last_seq: i64,
}

/// Read-only connections for queries.
///
/// Connection 0 is preferred whenever it is idle, so sequential queries reuse
/// one warm page cache instead of filling every connection's in turn; the
/// others serve concurrent queries. A connection that goes a whole writer
/// heartbeat interval (15 s) unused has its page cache released
/// ([`ReaderPool::release_idle`]), so a burst of history queries does not pin
/// up to `READERS ×` 8 MB of cache for the rest of the daemon's life. Caches
/// stay warm for the duration of a burst.
pub(crate) struct ReaderPool {
    conns: Vec<ReaderConn>,
    next: AtomicUsize,
}

struct ReaderConn {
    conn: Mutex<Connection>,
    /// Used since the last [`ReaderPool::release_idle`] tick.
    used: AtomicBool,
    /// May hold cached pages (used since its cache was last released).
    cached: AtomicBool,
}

impl ReaderPool {
    fn new(conns: Vec<Connection>) -> Self {
        Self {
            conns: conns
                .into_iter()
                .map(|c| ReaderConn {
                    conn: Mutex::new(c),
                    used: AtomicBool::new(false),
                    cached: AtomicBool::new(false),
                })
                .collect(),
            next: AtomicUsize::new(0),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        for rc in &self.conns {
            if let Ok(mut guard) = rc.conn.try_lock() {
                return Self::run(rc, &mut guard, f);
            }
        }
        // All busy: queue on one, round-robin.
        let n = self.conns.len().max(1);
        let pick = self.next.fetch_add(1, Ordering::Relaxed) % n;
        let rc = self.conns.get(pick).ok_or(StorageError::WriterStopped)?;
        let mut guard = rc.conn.lock().unwrap_or_else(|p| p.into_inner());
        Self::run(rc, &mut guard, f)
    }

    fn run<T>(
        rc: &ReaderConn,
        conn: &mut Connection,
        f: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        rc.used.store(true, Ordering::Relaxed);
        rc.cached.store(true, Ordering::Relaxed);
        f(conn)
    }

    /// Release the page cache of every connection unused since the previous
    /// call. Never waits: a connection that is busy right now is skipped.
    pub(crate) fn release_idle(&self) {
        for rc in &self.conns {
            if rc.used.swap(false, Ordering::Relaxed) || !rc.cached.load(Ordering::Relaxed) {
                continue;
            }
            let Ok(guard) = rc.conn.try_lock() else {
                continue;
            };
            match guard.release_memory() {
                Ok(()) => rc.cached.store(false, Ordering::Relaxed),
                Err(e) => tracing::debug!(err = %e, "reader cache release failed"),
            }
        }
    }
}

struct Inner {
    path: PathBuf,
    config: StorageConfig,
    policy: Arc<CapturePolicy>,
    project_id: Option<String>,
    tx: SyncSender<Command>,
    shared: Arc<Shared>,
    readers: Arc<ReaderPool>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    run_id: i64,
    run_key: String,
    started_at: DateTime<Utc>,
    recovered: Vec<RecoveredRun>,
    fts_enabled: bool,
    fts_from_seq: i64,
}

/// Cloneable handle to the store.
#[derive(Clone)]
pub struct Storage {
    inner: Arc<Inner>,
}

/// Resolve the configured database path.
///
/// # Errors
/// No path configured and no home directory to derive one from.
pub fn resolve_path(config: &StorageConfig) -> Result<PathBuf> {
    match &config.path {
        Some(p) => Ok(p.clone()),
        None => paths::default_db_path().ok_or_else(|| {
            StorageError::Config(
                "no home directory; set [storage] path or TAILFLOW_DATA_DIR".into(),
            )
        }),
    }
}

fn random_key() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Uniqueness, not secrecy, is what matters for a run key.
        let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a process with `pid` exists. Errs toward "alive": declaring a
/// live daemon dead would record a false gap, while the heartbeat-staleness
/// rule still catches a dead one later.
fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        // Never a real daemon pid; `kill -0 -1` would probe every process.
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // `ps -p` succeeds for any existing process regardless of owner
        // (unlike `kill -0`, which fails with EPERM). A probe that cannot
        // run at all is treated as "alive".
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) => status.success(),
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        // No cheap portable probe: rely on heartbeat staleness alone.
        let _ = pid;
        true
    }
}

/// Close runs whose process is gone (or whose heartbeat is stale) and
/// record an unclean-shutdown gap for each. Returns what it found.
pub(crate) fn recover_dead_runs(
    conn: &Connection,
    own_run: Option<i64>,
) -> Result<Vec<RecoveredRun>> {
    let now = Utc::now();
    let stale_before =
        now.timestamp_micros() - i64::try_from(STALE_RUN.as_micros()).unwrap_or(i64::MAX);
    let open: Vec<(i64, i64, i64, i64)> = {
        let mut stmt = conn
            .prepare("SELECT id, heartbeat_at, last_seq, pid FROM runs WHERE ended_at IS NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut found = Vec::new();
    for (id, heartbeat, last_seq, pid) in open {
        if Some(id) == own_run {
            continue;
        }
        let dead = heartbeat < stale_before || !pid_alive(pid);
        if !dead {
            continue;
        }
        let hb = DateTime::<Utc>::from_timestamp_micros(heartbeat);
        retention::immediate(conn, |c| {
            let changed = c.execute(
                "UPDATE runs SET ended_at = heartbeat_at, clean = 0 WHERE id = ?1 AND ended_at IS NULL",
                [id],
            )?;
            if changed == 0 {
                return Ok(()); // another daemon recovered it first
            }
            // Its filesystem observers stopped when it died (at the latest).
            c.execute(
                "UPDATE fs_coverage SET stopped_at = ?2 WHERE run_id = ?1 AND stopped_at IS NULL",
                rusqlite::params![id, heartbeat],
            )?;
            let first = hb.unwrap_or(now);
            // Cover everything committed since the dead run's last commit:
            // with a shared database another daemon may have written past
            // it, and the lost events belong somewhere in that interval.
            let (newest_seq, mut floor) = writer::newest(c)?;
            writer::record_gap_range(
                c,
                LossReason::UncleanShutdown,
                None,
                first,
                now,
                last_seq,
                newest_seq.max(last_seq),
                Some(id),
                Some("previous daemon run ended without a clean shutdown; events in flight were lost"),
            )?;
            let marker = writer::loss_marker(LossReason::UncleanShutdown, None, first, now);
            if writer::insert_event(c, &marker, &mut floor, id, true)?.is_some() {
                schema::meta_add(c, "event_count", 1)?;
            }
            Ok(())
        })?;
        tracing::warn!(run_id = id, last_seq, "recovered unclean previous run");
        found.push(RecoveredRun {
            run_id: id,
            last_heartbeat_at: hb,
            last_seq,
        });
    }
    Ok(found)
}

/// Reconcile the FTS index with the config. Returns `(enabled, from_seq)`:
/// text search covers seq ≥ `from_seq`.
fn reconcile_fts(conn: &Connection, enabled: bool) -> Result<(bool, i64)> {
    // FTS5 otherwise merges tombstoned segments synchronously when rows are
    // deleted; on a multi-million-row index one retention COMMIT took 24 s
    // (M1 benchmark). Retention runs bounded `merge` steps instead.
    conn.execute(
        "INSERT INTO events_fts(events_fts, rank) VALUES ('deletemerge', 0)",
        [],
    )?;
    let prev = schema::meta_get(conn, "fts_enabled")?;
    let (max_seq, _) = writer::newest(conn)?;
    if enabled {
        if prev.as_deref() != Some("1") {
            retention::immediate(conn, |c| {
                schema::meta_set(c, "fts_enabled", "1")?;
                schema::meta_set(c, "fts_from_seq", &(max_seq + 1).to_string())?;
                Ok(())
            })?;
        }
        let from = schema::meta_get_i64(conn, "fts_from_seq")?.max(1);
        Ok((true, from))
    } else {
        if prev.as_deref() == Some("1") {
            retention::immediate(conn, |c| {
                c.execute("DELETE FROM events_fts", [])?;
                schema::meta_set(c, "fts_enabled", "0")?;
                Ok(())
            })?;
        }
        Ok((false, i64::MAX))
    }
}

impl Storage {
    /// Open or create the store and start the writer.
    ///
    /// Never deletes, resets, or rewrites an existing database on failure.
    ///
    /// # Errors
    /// See [`StorageError`]; the daemon keeps running memory-only on any of them.
    pub fn open(config: &StorageConfig, opts: OpenOptions) -> Result<Self> {
        Self::open_with_writer_hook(config, opts, |_| Ok(()))
    }

    /// [`Storage::open`], running `hook` on the writer connection before the
    /// writer starts. Fault-injection seam for tests (e.g. `PRAGMA
    /// max_page_count` to simulate a full disk); not part of the stable API.
    #[doc(hidden)]
    pub fn open_with_writer_hook(
        config: &StorageConfig,
        opts: OpenOptions,
        hook: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<Self> {
        config
            .validate()
            .map_err(|e| StorageError::Config(e.to_string()))?;
        if !config.enabled {
            return Err(StorageError::Config("storage is disabled".into()));
        }
        let path = resolve_path(config)?;
        let conn = schema::open_writer(&path, config.synchronous)?;

        let recovered = recover_dead_runs(&conn, None)?;

        let started_at = Utc::now();
        let run_key = random_key();
        // A project path longer than the id bound would otherwise make every
        // event fail validation.
        let mut opts = opts;
        opts.project_id = opts
            .project_id
            .as_deref()
            .map(tailflow_core::event::bounded_id);
        conn.execute(
            "INSERT INTO runs (run_key, started_at, heartbeat_at, version, pid, project_id)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5)",
            params![
                run_key,
                started_at.timestamp_micros(),
                opts.version,
                i64::from(std::process::id()),
                opts.project_id
            ],
        )?;
        let run_id = conn.last_insert_rowid();
        let (fts_enabled, fts_from_seq) = reconcile_fts(&conn, config.full_text_search)?;
        hook(&conn)?;

        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            readers.push(schema::open_reader(&path)?);
        }
        let readers = Arc::new(ReaderPool::new(readers));

        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::sync_channel(config.queue_capacity);
        let wcfg = WriterConfig {
            batch_max: config.batch_max_events,
            batch_delay: Duration::from_millis(config.batch_max_delay_ms),
            fts: fts_enabled,
            policy: RetentionPolicy {
                max_age_days: config.retention_days.days(),
                payload_max_age_days: config.payload_retention_days,
                max_bytes: config.max_size_mb.saturating_mul(1024 * 1024),
            },
            cleanup_interval: Duration::from_secs(config.cleanup_interval_secs),
            heartbeat_interval: HEARTBEAT,
            retention_budget: Duration::from_millis(250),
            queue_capacity: config.queue_capacity,
        };
        let writer = Writer {
            conn,
            rx,
            shared: shared.clone(),
            cfg: wcfg,
            run_id,
            // Startup cleanup happens right away, before the first interval.
            next_cleanup: Instant::now(),
            next_heartbeat: Instant::now() + HEARTBEAT,
            resuming_cleanup: false,
            cycle: retention::Cycle::default(),
            readers: readers.clone(),
            fts_owed: false,
            deferred_since: None,
        };
        let handle = std::thread::Builder::new()
            .name("tailflow-storage".into())
            .spawn(move || writer.run())
            .map_err(StorageError::Io)?;

        if !recovered.is_empty() {
            // WAL replay already happened on open. `quick_check` is
            // O(database) — about 30 s on a 1.2 GB file — so it runs on its
            // own read connection in the background instead of delaying
            // startup; the result is reported in status.
            let (check_path, check_shared) = (path.clone(), shared.clone());
            let spawned = std::thread::Builder::new()
                .name("tailflow-storage-check".into())
                .spawn(move || {
                    let result = schema::open_reader(&check_path)
                        .and_then(|c| schema::integrity_problems(&c, false));
                    let problems = match result {
                        Ok(p) => p,
                        Err(e) => vec![format!("integrity check could not run: {e}")],
                    };
                    if !problems.is_empty() {
                        tracing::error!(problems = ?problems, "storage integrity check failed");
                        check_shared.health.set_error(format!(
                            "integrity check after unclean shutdown failed: {}",
                            problems.join("; ")
                        ));
                    }
                    *check_shared
                        .integrity
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some((Utc::now(), problems));
                });
            if let Err(e) = spawned {
                tracing::warn!(err = %e, "could not start the background integrity check");
            }
        }

        Ok(Self {
            inner: Arc::new(Inner {
                path,
                config: config.clone(),
                policy: opts.policy,
                project_id: opts.project_id,
                tx,
                shared,
                readers,
                writer: Mutex::new(Some(handle)),
                run_id,
                run_key,
                started_at,
                recovered,
                fts_enabled,
                fts_from_seq,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn run_key(&self) -> &str {
        &self.inner.run_key
    }

    pub fn project_id(&self) -> Option<&str> {
        self.inner.project_id.as_deref()
    }

    pub fn policy(&self) -> &CapturePolicy {
        &self.inner.policy
    }

    pub fn recovered_runs(&self) -> &[RecoveredRun] {
        &self.inner.recovered
    }

    /// Offer one event for persistence. Never blocks.
    ///
    /// Applies the capture policy (collection boundaries, capture mode,
    /// redaction) **before** the event enters the queue, so nothing
    /// unredacted can reach disk regardless of the caller.
    pub fn submit(&self, event: ObservabilityEvent) -> Submitted {
        self.submit_inner(event, None)
    }

    /// [`Storage::submit`] for producers that can wait (the agent spool
    /// ingestor): when the queue is full it retries until `deadline` instead
    /// of dropping at once, so a burst of replayed agent events applies
    /// backpressure to the spool rather than becoming loss. Blocking: call
    /// from a blocking context. Loss after the deadline is counted and
    /// recorded exactly as for `submit`.
    pub fn submit_backpressured(&self, event: ObservabilityEvent, deadline: Instant) -> Submitted {
        self.submit_inner(event, Some(deadline))
    }

    fn submit_inner(&self, mut event: ObservabilityEvent, wait: Option<Instant>) -> Submitted {
        let h = &self.inner.shared.health;
        h.received.fetch_add(1, Ordering::Relaxed);
        h.last_event_at_us
            .store(event.observed_at.timestamp_micros(), Ordering::Relaxed);
        if event.project_id.is_none() {
            event.project_id.clone_from(&self.inner.project_id);
        }
        if self.inner.policy.admit(&event).is_err() {
            h.excluded.fetch_add(1, Ordering::Relaxed);
            return Submitted::Excluded;
        }
        self.inner.policy.sanitize(&mut event);
        let observed = event.observed_at;
        if event.validate().is_err() {
            // A rejected event is still an event TailFlow saw and did not
            // keep: count it as loss so history never looks complete.
            h.invalid.fetch_add(1, Ordering::Relaxed);
            self.inner
                .shared
                .add_loss(LossReason::InvalidEvent, 1, observed, observed);
            return Submitted::Invalid;
        }
        if self.inner.shared.stopping.load(Ordering::SeqCst) {
            h.dropped_writer_stopped.fetch_add(1, Ordering::Relaxed);
            self.inner
                .shared
                .add_loss(LossReason::WriterStopped, 1, observed, observed);
            return Submitted::Dropped(LossReason::WriterStopped);
        }
        // Counted before the send so the writer's final drain can see that
        // something is still in flight.
        h.queue_depth.fetch_add(1, Ordering::SeqCst);
        let mut cmd = Command::Event(Box::new(event));
        let sent = loop {
            match self.inner.tx.try_send(cmd) {
                Err(TrySendError::Full(back))
                    if wait.is_some_and(|d| Instant::now() < d)
                        && !self.inner.shared.stopping.load(Ordering::SeqCst) =>
                {
                    cmd = back;
                    std::thread::sleep(Duration::from_millis(2));
                }
                other => break other,
            }
        };
        match sent {
            Ok(()) => {
                h.enqueued.fetch_add(1, Ordering::Relaxed);
                Submitted::Enqueued
            }
            Err(TrySendError::Full(_)) => {
                h.queue_depth.fetch_sub(1, Ordering::SeqCst);
                h.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                self.inner
                    .shared
                    .add_loss(LossReason::QueueFull, 1, observed, observed);
                Submitted::Dropped(LossReason::QueueFull)
            }
            Err(TrySendError::Disconnected(_)) => {
                h.queue_depth.fetch_sub(1, Ordering::SeqCst);
                h.dropped_writer_stopped.fetch_add(1, Ordering::Relaxed);
                self.inner
                    .shared
                    .add_loss(LossReason::WriterStopped, 1, observed, observed);
                Submitted::Dropped(LossReason::WriterStopped)
            }
        }
    }

    /// Record loss detected upstream of storage (e.g. the ingestion bus
    /// lagged). Written as a gap with the next batch.
    pub fn record_upstream_loss(&self, count: u64, first: DateTime<Utc>, last: DateTime<Utc>) {
        self.inner
            .shared
            .health
            .dropped_ingest_lag
            .fetch_add(count, Ordering::Relaxed);
        self.inner
            .shared
            .add_loss(LossReason::IngestLag, count, first, last);
        // Nudge an idle writer so the gap is recorded promptly.
        let (ack, _rx) = mpsc::sync_channel(1);
        let _queue_full_is_fine = self.inner.tx.try_send(Command::Flush(ack));
    }

    /// Record agent-side loss reported through the spool (overflow,
    /// rejected input, corrupt records). Written as a gap with the next
    /// batch, plus a `system.events.dropped` marker.
    pub fn record_agent_loss(
        &self,
        reason: LossReason,
        count: u64,
        first: DateTime<Utc>,
        last: DateTime<Utc>,
    ) {
        if count == 0 {
            return;
        }
        self.inner
            .shared
            .health
            .dropped_agent
            .fetch_add(count, Ordering::Relaxed);
        self.inner.shared.add_loss(reason, count, first, last);
    }

    /// Record filesystem-observer loss ([`LossReason::FsObserverOverflow`]
    /// or [`LossReason::FsWatcherRescan`]). Written as a gap with the next
    /// batch, plus a `system.events.dropped` marker.
    pub fn record_observer_loss(
        &self,
        reason: LossReason,
        count: u64,
        first: DateTime<Utc>,
        last: DateTime<Utc>,
    ) {
        if count == 0 {
            return;
        }
        self.inner
            .shared
            .health
            .dropped_fs_observer
            .fetch_add(count, Ordering::Relaxed);
        self.inner.shared.add_loss(reason, count, first, last);
        let (ack, _rx) = mpsc::sync_channel(1);
        let _queue_full_is_fine = self.inner.tx.try_send(Command::Flush(ack));
    }

    /// Claim filesystem observation of `root` for this run. Two daemons
    /// sharing the database must not both observe one directory (every
    /// change would be recorded twice): the claim fails while another live
    /// run (heartbeat within the last minute) holds an open claim on the
    /// same root. Check and insert happen in one write transaction.
    /// Blocking.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn coverage_started(
        &self,
        root: &str,
        backend: &str,
        watched_dirs: u64,
        unwatched_dirs: u64,
    ) -> Result<CoverageClaim> {
        let conn = schema::open_writer(&self.inner.path, self.inner.config.synchronous)?;
        let run_id = self.inner.run_id;
        let fresh = Utc::now().timestamp_micros()
            - i64::try_from(STALE_RUN.as_micros()).unwrap_or(i64::MAX);
        retention::immediate(&conn, |c| {
            let held: Option<(i64, i64)> = c
                .query_row(
                    "SELECT f.run_id, r.pid FROM fs_coverage f JOIN runs r ON r.id = f.run_id
                     WHERE f.root = ?1 AND f.stopped_at IS NULL AND f.run_id != ?2
                       AND r.ended_at IS NULL AND r.heartbeat_at >= ?3
                     LIMIT 1",
                    params![root, run_id, fresh],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((other_run, pid)) = held {
                return Ok(CoverageClaim::HeldBy {
                    run_id: other_run,
                    pid,
                });
            }
            c.execute(
                "INSERT INTO fs_coverage (root, run_id, started_at, backend, watched_dirs, unwatched_dirs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    root,
                    run_id,
                    Utc::now().timestamp_micros(),
                    backend,
                    i64::try_from(watched_dirs).unwrap_or(i64::MAX),
                    i64::try_from(unwatched_dirs).unwrap_or(i64::MAX)
                ],
            )?;
            Ok(CoverageClaim::Claimed(c.last_insert_rowid()))
        })
    }

    /// Update the directory counts of a running observation (directories
    /// added or over budget since it started). Blocking.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn coverage_update(&self, id: i64, watched_dirs: u64, unwatched_dirs: u64) -> Result<()> {
        let conn = schema::open_writer(&self.inner.path, self.inner.config.synchronous)?;
        conn.execute(
            "UPDATE fs_coverage SET watched_dirs = ?2, unwatched_dirs = ?3 WHERE id = ?1",
            params![
                id,
                i64::try_from(watched_dirs).unwrap_or(i64::MAX),
                i64::try_from(unwatched_dirs).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// Record that observation `id` stopped. Blocking.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn coverage_stopped(&self, id: i64) -> Result<()> {
        let conn = schema::open_writer(&self.inner.path, self.inner.config.synchronous)?;
        conn.execute(
            "UPDATE fs_coverage SET stopped_at = ?2 WHERE id = ?1 AND stopped_at IS NULL",
            params![id, Utc::now().timestamp_micros()],
        )?;
        Ok(())
    }

    /// Count events a hook's capture policy excluded before spooling
    /// (deliberate, not loss), so `events_excluded` covers them.
    pub fn add_excluded(&self, n: u64) {
        let h = &self.inner.shared.health;
        h.received.fetch_add(n, Ordering::Relaxed);
        h.excluded.fetch_add(n, Ordering::Relaxed);
    }

    /// Run `hook` on the writer thread after every commit that inserted
    /// events. It only says "something was committed": readers find out
    /// what by querying with a seq cursor. Commits made by *other*
    /// processes sharing the database do not call it; poll
    /// [`Storage::newest_seq`] for those. The hook must be cheap and must
    /// not block (it delays the next commit).
    pub fn on_commit(&self, hook: impl Fn(&CommitInfo) + Send + Sync + 'static) {
        self.inner
            .shared
            .commit_hooks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::new(hook));
    }

    /// The newest committed seq by any writer (0 when empty). One index
    /// lookup. Blocking.
    ///
    /// Commits are serialized by SQLite's write lock and `seq` is assigned
    /// under it, so committed seqs only grow: once a reader has seen seq
    /// `n`, no commit by any process can later add a seq below `n`. A
    /// `seq > cursor` tail therefore never misses a committed event.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn newest_seq(&self) -> Result<u64> {
        self.inner.readers.with(|c| {
            let seq: Option<i64> = c
                .query_row(
                    "SELECT seq FROM events ORDER BY seq DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(seq.and_then(|s| u64::try_from(s).ok()).unwrap_or(0))
        })
    }

    /// Agent sessions, most recent first. Blocking.
    ///
    /// # Errors
    /// Invalid filters or a SQLite failure.
    pub fn sessions(&self, q: &sessions::SessionQuery) -> Result<sessions::SessionPage> {
        self.inner.readers.with(|c| sessions::list(c, q))
    }

    /// Overview aggregates for the dashboard. Blocking.
    ///
    /// # Errors
    /// Invalid filters, the query time budget, or a SQLite failure.
    pub fn stats(&self, q: &stats::StatsQuery) -> Result<stats::Stats> {
        self.inner.readers.with(|c| stats::overview(c, q))
    }

    /// Agent-reported file activity grouped by path. Blocking.
    ///
    /// # Errors
    /// Invalid filters, the query time budget, or a SQLite failure.
    pub fn files(&self, q: &stats::FilesQuery) -> Result<stats::FilesPage> {
        self.inner.readers.with(|c| stats::files(c, q))
    }

    /// Agent-reported file changes compared with filesystem observations.
    /// Blocking.
    ///
    /// # Errors
    /// Invalid filters, the query time budget, or a SQLite failure.
    pub fn file_evidence(
        &self,
        q: &evidence::FileEvidenceQuery,
    ) -> Result<evidence::FileEvidencePage> {
        self.inner.readers.with(|c| evidence::files(c, q))
    }

    /// Run `f` on a pooled read connection (for daemon-side read helpers
    /// that need several statements). Blocking.
    ///
    /// # Errors
    /// Whatever `f` returns.
    pub fn read<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        self.inner.readers.with(f)
    }

    /// One agent session with summaries. Blocking.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn session(&self, key: &str) -> Result<Option<sessions::SessionDetail>> {
        self.inner.readers.with(|c| sessions::detail(c, key))
    }

    fn send_control(&self, cmd: Command, deadline: Instant) -> Result<()> {
        let mut cmd = cmd;
        loop {
            match self.inner.tx.try_send(cmd) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(StorageError::WriterStopped),
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return Err(StorageError::QueryTimeout);
                    }
                    cmd = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Block until everything submitted before this call is committed or
    /// accounted for as lost. Blocking: call from a blocking context.
    ///
    /// # Errors
    /// Writer stopped, or `timeout` elapsed.
    pub fn flush(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let (ack, rx) = mpsc::sync_channel(1);
        self.send_control(Command::Flush(ack), deadline)?;
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| StorageError::QueryTimeout)
    }

    /// Run a retention pass now (normally periodic). Blocking.
    ///
    /// # Errors
    /// Writer stopped, timeout, or a SQLite error during cleanup.
    pub fn run_retention(&self, timeout: Duration) -> Result<RetentionReport> {
        let deadline = Instant::now() + timeout;
        let (ack, rx) = mpsc::sync_channel(1);
        self.send_control(Command::RetentionNow(ack), deadline)?;
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| StorageError::QueryTimeout)?
    }

    /// Drain the queue, commit, mark this run clean, and stop the writer.
    /// Idempotent. Blocking.
    ///
    /// # Errors
    /// The writer did not acknowledge within `timeout`; the run will then be
    /// recovered as unclean on next start.
    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        let Some(handle) = self
            .inner
            .writer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        else {
            return Ok(());
        };
        self.inner.shared.stopping.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        let (ack, rx) = mpsc::sync_channel(1);
        self.send_control(Command::Shutdown(ack), deadline)?;
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| StorageError::QueryTimeout)?;
        handle.join().map_err(|_| StorageError::WriterStopped)?;
        self.record_late_loss()
    }

    /// Loss detected after the writer's final commit (a submit racing
    /// shutdown) must still become durable: write it directly, or — if even
    /// that fails — reopen the run so the next start records an unclean gap.
    fn record_late_loss(&self) -> Result<()> {
        let late = self
            .inner
            .shared
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if late.is_empty() {
            return Ok(());
        }
        let conn = schema::open_writer(&self.inner.path, self.inner.config.synchronous)?;
        let run_id = self.inner.run_id;
        let written = retention::immediate(&conn, |c| {
            let (at, mut floor) = writer::newest(c)?;
            for (reason, loss) in &late {
                writer::record_gap(
                    c,
                    *reason,
                    Some(loss.count),
                    loss.first_at,
                    loss.last_at,
                    at,
                    Some(run_id),
                    None,
                )?;
                let marker =
                    writer::loss_marker(*reason, Some(loss.count), loss.first_at, loss.last_at);
                if writer::insert_event(c, &marker, &mut floor, run_id, self.inner.fts_enabled)?
                    .is_some()
                {
                    schema::meta_add(c, "event_count", 1)?;
                }
            }
            Ok(())
        });
        if written.is_ok() {
            // Recorded: the run is over, and honestly not clean.
            conn.execute(
                "UPDATE runs SET ended_at = ?1, clean = 0 WHERE id = ?2",
                rusqlite::params![Utc::now().timestamp_micros(), run_id],
            )?;
        } else {
            conn.execute(
                "UPDATE runs SET ended_at = NULL, clean = 0 WHERE id = ?1",
                [run_id],
            )?;
        }
        written
    }

    pub fn health(&self) -> HealthSnapshot {
        self.inner
            .shared
            .health
            .snapshot(self.inner.shared.pending_total())
    }

    fn ctx(&self) -> query::QueryContext {
        query::QueryContext {
            fts_enabled: self.inner.fts_enabled,
            fts_from_seq: self.inner.fts_from_seq,
            pending_loss: self.inner.shared.pending_total(),
        }
    }

    /// Run a historical query. Blocking.
    ///
    /// # Errors
    /// Invalid query, time budget exceeded, or SQLite failure.
    pub fn query(&self, q: &EventQuery) -> Result<EventPage> {
        let ctx = self.ctx();
        self.inner.readers.with(|c| query::execute(c, q, &ctx))
    }

    /// One event by seq. Blocking.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn get(&self, seq: u64, include_payload: bool) -> Result<Option<StoredEvent>> {
        self.inner
            .readers
            .with(|c| query::get(c, seq, include_payload))
    }

    /// Whether `seq` is below the retention horizon (expired, not unknown).
    ///
    /// # Errors
    /// SQLite failure.
    pub fn is_expired(&self, seq: u64) -> Result<bool> {
        self.inner.readers.with(|c| {
            let evicted = schema::meta_get_i64(c, "evicted_through_seq")?;
            Ok(i64::try_from(seq).is_ok_and(|s| s <= evicted))
        })
    }

    /// Database and health summary. Blocking.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn status(&self) -> Result<StorageStatus> {
        let health = self.health();
        let last_retention = self
            .inner
            .shared
            .last_retention
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let cfg = &self.inner.config;
        self.inner.readers.with(|c| {
            let db = db_stats(c, &self.inner.path)?;
            Ok(StorageStatus {
                enabled: true,
                state: health.writer_state,
                path: self.inner.path.display().to_string(),
                run_key: self.inner.run_key.clone(),
                run_id: self.inner.run_id,
                started_at: self.inner.started_at,
                db,
                retention: RetentionView {
                    retention_days: cfg.retention_days.days(),
                    unlimited_age: cfg.retention_days.days().is_none(),
                    payload_retention_days: cfg.payload_retention_days,
                    max_size_mb: cfg.max_size_mb,
                    cleanup_interval_secs: cfg.cleanup_interval_secs,
                    last_cleanup_at: last_retention.as_ref().map(|(t, _)| *t),
                    last_cleanup: last_retention.map(|(_, r)| r),
                },
                full_text_search: self.inner.fts_enabled,
                text_index_from_seq: self
                    .inner
                    .fts_enabled
                    .then(|| u64::try_from(self.inner.fts_from_seq).unwrap_or(0)),
                capture_mode: self.inner.policy.mode.as_str(),
                capture_enabled: self.inner.policy.enabled,
                synchronous: match cfg.synchronous {
                    tailflow_core::config::SyncMode::Normal => "normal",
                    tailflow_core::config::SyncMode::Full => "full",
                },
                recovered_unclean_runs: self.inner.recovered.len(),
                integrity_check: self
                    .inner
                    .shared
                    .integrity
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .map(|(at, problems)| IntegrityView {
                        checked_at: *at,
                        ok: problems.is_empty(),
                        problems: problems.clone(),
                    }),
                health,
            })
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Dropping the sender lets the writer see `Disconnected`, finish, and
        // mark the run clean. Join so the process does not exit mid-commit.
        if let Some(handle) = self
            .writer
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let (tx, _) = mpsc::sync_channel(0);
            drop(std::mem::replace(&mut self.tx, tx));
            let _writer_panicked = handle.join();
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DbStats {
    pub events: u64,
    pub oldest_seq: Option<u64>,
    pub newest_seq: Option<u64>,
    pub oldest_event_at: Option<DateTime<Utc>>,
    pub newest_event_at: Option<DateTime<Utc>>,
    pub evicted_through_seq: u64,
    pub gaps: u64,
    pub last_gap: Option<query::GapView>,
    pub schema_version: i32,
    pub sqlite_version: String,
    pub file_bytes: u64,
    pub wal_bytes: u64,
    /// Live pages (file minus free pages): what the size cap is enforced on.
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionView {
    pub retention_days: Option<u32>,
    pub unlimited_age: bool,
    pub payload_retention_days: u32,
    pub max_size_mb: u64,
    pub cleanup_interval_secs: u64,
    pub last_cleanup_at: Option<DateTime<Utc>>,
    pub last_cleanup: Option<RetentionReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageStatus {
    pub enabled: bool,
    pub state: WriterState,
    pub path: String,
    pub run_key: String,
    pub run_id: i64,
    pub started_at: DateTime<Utc>,
    pub db: DbStats,
    pub retention: RetentionView,
    pub full_text_search: bool,
    pub text_index_from_seq: Option<u64>,
    pub capture_mode: &'static str,
    pub capture_enabled: bool,
    pub synchronous: &'static str,
    pub recovered_unclean_runs: usize,
    /// Present only after an unclean shutdown triggered a background check.
    pub integrity_check: Option<IntegrityView>,
    pub health: HealthSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntegrityView {
    pub checked_at: DateTime<Utc>,
    pub ok: bool,
    pub problems: Vec<String>,
}

/// Counters and sizes, cheap enough for a status endpoint: every value is an
/// O(1) or O(log n) lookup, never a table scan.
pub fn db_stats(c: &Connection, path: &Path) -> Result<DbStats> {
    let pages = retention::page_stats(c)?;
    let oldest: Option<(i64, i64)> = c
        .query_row(
            "SELECT seq, observed_at FROM events ORDER BY seq ASC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let newest: Option<(i64, i64)> = c
        .query_row(
            "SELECT seq, observed_at FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let last_gap = c
        .query_row(
            "SELECT reason, count, first_at, last_at, from_seq, to_seq FROM gaps ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok(query::GapView {
                    reason: r.get(0)?,
                    count: r.get::<_, Option<i64>>(1)?.and_then(|n| u64::try_from(n).ok()),
                    first_at: DateTime::from_timestamp_micros(r.get(2)?),
                    last_at: DateTime::from_timestamp_micros(r.get(3)?),
                    from_seq: u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                    to_seq: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
                })
            },
        )
        .optional()?;
    let wal = {
        let mut p = path.as_os_str().to_owned();
        p.push("-wal");
        std::fs::metadata(PathBuf::from(p)).map_or(0, |m| m.len())
    };
    let to_u = |v: i64| u64::try_from(v).unwrap_or(0);
    Ok(DbStats {
        events: to_u(schema::meta_get_i64(c, "event_count")?),
        oldest_seq: oldest.map(|(s, _)| to_u(s)),
        newest_seq: newest.map(|(s, _)| to_u(s)),
        oldest_event_at: oldest.and_then(|(_, t)| DateTime::from_timestamp_micros(t)),
        newest_event_at: newest.and_then(|(_, t)| DateTime::from_timestamp_micros(t)),
        evicted_through_seq: to_u(schema::meta_get_i64(c, "evicted_through_seq")?),
        gaps: to_u(c.query_row("SELECT count(*) FROM gaps", [], |r| r.get(0))?),
        last_gap,
        schema_version: c.pragma_query_value(None, "user_version", |r| r.get(0))?,
        sqlite_version: rusqlite::version().to_string(),
        file_bytes: pages.file_bytes(),
        wal_bytes: wal,
        used_bytes: pages.used_bytes(),
        free_bytes: pages.freelist_count.saturating_mul(pages.page_size),
    })
}

/// Offline maintenance used by `tailflow-daemon storage …`. Each opens its
/// own connection and is safe to run while a daemon is writing: SQLite
/// serializes the writers and the daemon's next query sees the result.
pub mod admin {
    use super::{db_stats, retention, schema, writer, DbStats, LossReason, Result};
    use chrono::{DateTime, Utc};
    use rusqlite::{Connection, OptionalExtension};
    use serde::Serialize;
    use std::path::Path;
    use tailflow_core::config::SyncMode;

    #[derive(Debug, Clone, Copy)]
    pub enum PurgeTarget<'a> {
        All,
        /// Events received before this instant.
        Before(DateTime<Utc>),
        Session(&'a str),
    }

    #[derive(Debug, Clone, Default, Serialize)]
    pub struct PurgeReport {
        pub deleted: u64,
        pub vacuumed_pages: u64,
    }

    fn open(path: &Path) -> Result<Connection> {
        schema::open_writer(path, SyncMode::Normal)
    }

    /// # Errors
    /// Open or SQLite failure.
    pub fn status(path: &Path) -> Result<DbStats> {
        db_stats(&open(path)?, path)
    }

    /// A historical query read directly from the file (no daemon needed).
    ///
    /// # Errors
    /// Open failure, invalid query, or SQLite failure.
    pub fn query(path: &Path, q: &crate::EventQuery) -> Result<crate::EventPage> {
        let mut conn = schema::open_reader(path)?;
        let fts_enabled = schema::meta_get(&conn, "fts_enabled")?.as_deref() == Some("1");
        let fts_from_seq = if fts_enabled {
            schema::meta_get_i64(&conn, "fts_from_seq")?.max(1)
        } else {
            i64::MAX
        };
        let ctx = crate::query::QueryContext {
            fts_enabled,
            fts_from_seq,
            pending_loss: 0,
        };
        crate::query::execute(&mut conn, q, &ctx)
    }

    /// Agent sessions, read directly from the file (no daemon needed).
    ///
    /// # Errors
    /// Open failure, invalid filters, or SQLite failure.
    pub fn sessions(
        path: &Path,
        q: &crate::sessions::SessionQuery,
    ) -> Result<crate::sessions::SessionPage> {
        let conn = schema::open_writer(path, SyncMode::Normal)?;
        crate::sessions::list(&conn, q)
    }

    /// One agent session, read directly from the file.
    ///
    /// # Errors
    /// Open failure or SQLite failure.
    pub fn session(path: &Path, key: &str) -> Result<Option<crate::sessions::SessionDetail>> {
        let conn = schema::open_writer(path, SyncMode::Normal)?;
        crate::sessions::detail(&conn, key)
    }

    /// Full `PRAGMA integrity_check`. Empty = healthy. Read-only.
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn check(path: &Path) -> Result<Vec<String>> {
        let conn = schema::open_reader(path)?;
        schema::integrity_problems(&conn, true)
    }

    /// Delete history. Chunked transactions; never `DROP` or file removal.
    ///
    /// # Errors
    /// Open or SQLite failure (already-committed chunks stay deleted).
    pub fn purge(path: &Path, target: PurgeTarget<'_>) -> Result<PurgeReport> {
        let conn = open(path)?;
        let mut report = PurgeReport::default();
        match target {
            PurgeTarget::All => {
                // Only what existed when the command started: a daemon
                // writing meanwhile keeps its new events.
                let (upto, _) = writer::newest(&conn)?;
                loop {
                    let n = retention::delete_oldest_chunk(&conn, upto.saturating_add(1))?;
                    if n == 0 {
                        break;
                    }
                    report.deleted += n;
                }
            }
            PurgeTarget::Before(t) => {
                if let Some(upto) = retention::first_seq_at_or_after(&conn, t.timestamp_micros())? {
                    loop {
                        let n = retention::delete_oldest_chunk(&conn, upto)?;
                        if n == 0 {
                            break;
                        }
                        report.deleted += n;
                    }
                }
            }
            PurgeTarget::Session(id) => {
                report.deleted = delete_session(&conn, id)?;
            }
        }
        loop {
            let before = retention::page_stats(&conn)?.freelist_count;
            if before == 0 {
                break;
            }
            conn.execute_batch("PRAGMA incremental_vacuum(4096)")?;
            let after = retention::page_stats(&conn)?.freelist_count;
            report.vacuumed_pages += before.saturating_sub(after);
            if after >= before {
                break;
            }
        }
        let _checkpoint_best_effort: rusqlite::Result<(i64, i64, i64)> =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            });
        Ok(report)
    }

    /// Delete one session's events and record a `session_deleted` gap over
    /// the seq range they occupied, so queries over that range say so.
    pub(crate) fn delete_session(conn: &Connection, session: &str) -> Result<u64> {
        // The gap row is written in the same transaction as the first chunk
        // and its count grows with each later chunk, so an interrupted purge
        // never leaves deleted rows without a gap describing them.
        let first = retention::immediate(conn, |c| {
            let lo: Option<i64> = c
                .query_row(
                    "SELECT seq FROM events WHERE session_id = ?1 ORDER BY seq ASC LIMIT 1",
                    [session],
                    |r| r.get(0),
                )
                .optional()?;
            let hi: Option<i64> = c
                .query_row(
                    "SELECT seq FROM events WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                    [session],
                    |r| r.get(0),
                )
                .optional()?;
            let (Some(lo), Some(hi)) = (lo, hi) else {
                return Ok(None);
            };
            let now = Utc::now();
            writer::record_gap_range(
                c,
                LossReason::SessionDeleted,
                Some(0),
                now,
                now,
                lo,
                hi,
                None,
                None,
            )?;
            let gap = c.last_insert_rowid();
            // The summary goes in the same transaction as the gap: an
            // interrupted purge can leave some of the session's events, but
            // never a summary that still counts the deleted ones.
            c.execute(
                "DELETE FROM agent_sessions WHERE session_key = ?1",
                [session],
            )?;
            Ok(Some((gap, hi)))
        })?;
        let Some((gap_id, hi)) = first else {
            conn.execute(
                "DELETE FROM agent_sessions WHERE session_key = ?1",
                [session],
            )?;
            return Ok(0);
        };
        let mut total = 0u64;
        loop {
            let n = retention::immediate(conn, |c| {
                let ids: Vec<i64> = {
                    let mut stmt = c.prepare(
                        "SELECT seq FROM events WHERE session_id = ?1 AND seq <= ?2 ORDER BY seq LIMIT ?3",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![session, hi, retention::CHUNK], |r| {
                            r.get(0)
                        })?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                for id in &ids {
                    c.execute("DELETE FROM events_fts WHERE rowid = ?1", [id])?;
                    c.execute("DELETE FROM events WHERE seq = ?1", [id])?;
                }
                let n = i64::try_from(ids.len()).unwrap_or(i64::MAX);
                schema::meta_add(c, "event_count", -n)?;
                c.execute(
                    "UPDATE gaps SET count = count + ?1 WHERE id = ?2",
                    rusqlite::params![n, gap_id],
                )?;
                Ok(u64::try_from(n).unwrap_or(0))
            })?;
            if n == 0 {
                break;
            }
            total += n;
        }
        Ok(total)
    }
}
