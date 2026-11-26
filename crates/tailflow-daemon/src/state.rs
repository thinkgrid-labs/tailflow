use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tailflow_core::{
    event::{ObservabilityEvent, Provenance},
    privacy::{CapturePolicy, LineRedactor, PrivacyConfig},
    processor::Filter,
    query::{LogStore, SeqRecord, SourceStat},
    LogLevel, LogReceiver, LogRecord,
};
use tailflow_storage::Storage;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Source name used for records TailFlow itself injects into the live stream.
pub const SYSTEM_SOURCE: &str = "tailflow";

/// Per-source collector counters kept for at most this many distinct names.
/// Docker container names are ephemeral; beyond the cap new names are still
/// ingested and stored, just not given their own counter row.
const MAX_COLLECTOR_ENTRIES: usize = 1_024;

/// Durable storage as this daemon run sees it.
pub enum StorageSlot {
    Enabled(Storage),
    /// `[storage] enabled = false` or `--no-storage`.
    Disabled,
    /// Configured but could not be opened. The daemon runs memory-only.
    Unavailable {
        kind: &'static str,
        message: String,
    },
}

impl StorageSlot {
    pub fn storage(&self) -> Option<&Storage> {
        match self {
            StorageSlot::Enabled(s) => Some(s),
            _ => None,
        }
    }
}

/// Bounds of the in-memory ring (see [`LogStore::with_limits`]).
#[derive(Clone, Copy)]
pub struct RingLimits {
    pub records: usize,
    pub bytes: usize,
}

/// Everything the ingestion fan-out needs beyond the bus.
pub struct Pipeline {
    /// Daemon-level `--grep`/`--source` filter.
    pub filter: Filter,
    pub policy: Arc<CapturePolicy>,
    pub storage: StorageSlot,
    /// Prefix for persisted runtime event ids: `<run_key>:<legacy seq>`.
    pub run_key: String,
    /// Cancelled when the daemon starts shutting down.
    pub shutdown: CancellationToken,
    /// Agent spool ingestion status (shared with the ingest thread).
    pub agent_ingest: crate::agent_ingest::SharedStatus,
    /// API token, browser sessions and sign-in codes for this run.
    pub auth: Arc<crate::auth::Auth>,
}

/// Loss and throughput at the live ingestion point. These cover the ring,
/// SSE, and legacy endpoints — not only storage.
#[derive(Default)]
pub struct IngestStats {
    pub received: AtomicU64,
    /// Records the source bus discarded because the fan-out fell behind.
    pub dropped: AtomicU64,
    pub last_drop_at_us: AtomicI64,
    pub redacted: AtomicU64,
    /// Records lost by individual SSE subscribers (per client, summed).
    pub sse_lagged: AtomicU64,
}

#[derive(Serialize)]
pub struct IngestView {
    pub records_received: u64,
    pub records_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_drop_at: Option<DateTime<Utc>>,
    pub records_redacted: u64,
    pub sse_client_lag_records: u64,
}

impl IngestStats {
    pub fn view(&self) -> IngestView {
        let last = self.last_drop_at_us.load(Ordering::Relaxed);
        IngestView {
            records_received: self.received.load(Ordering::Relaxed),
            records_dropped: self.dropped.load(Ordering::Relaxed),
            last_drop_at: (last > 0)
                .then(|| DateTime::<Utc>::from_timestamp_micros(last))
                .flatten(),
            records_redacted: self.redacted.load(Ordering::Relaxed),
            sse_client_lag_records: self.sse_lagged.load(Ordering::Relaxed),
        }
    }
}

pub struct AppState {
    /// Live records. The ring and every subscriber share each record's
    /// allocation (`Arc`), so N SSE clients cost N pointers, not N copies.
    pub tx: broadcast::Sender<Arc<SeqRecord>>,
    pub store: Arc<LogStore>,
    pub storage: StorageSlot,
    pub policy: Arc<CapturePolicy>,
    pub ingest: Arc<IngestStats>,
    /// Cancelled when the daemon starts shutting down. Long-lived responses
    /// (SSE streams, `/api/wait`) end on it: the HTTP server's graceful
    /// shutdown waits for every open connection, so a stream that never
    /// ends would keep the daemon — and the storage drain behind it — from
    /// ever stopping.
    pub shutdown: CancellationToken,
    pub agent_ingest: crate::agent_ingest::SharedStatus,
    /// Access control: every route except the static UI shell, `/health`
    /// and the sign-in link requires a credential (see `auth.rs`).
    pub auth: Arc<crate::auth::Auth>,
    /// Identifies this daemon process. The live ring's `seq` restarts with
    /// every process, so a client compares this before resuming a cursor.
    pub instance_id: String,
    sources: Mutex<HashMap<String, RuntimeSource>>,
    collectors: Arc<Mutex<HashMap<String, CollectorCounters>>>,
}

#[derive(Clone)]
struct RuntimeSource {
    status: &'static str,
    kind: &'static str,
    detail: Option<String>,
    changed_at: DateTime<Utc>,
}

#[derive(Clone, Default)]
struct CollectorCounters {
    kind: &'static str,
    received: u64,
    last_event_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct SourceView {
    pub name: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub total: usize,
    pub errors: usize,
    pub warns: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_line: Option<String>,
    pub status_changed_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct CollectorView {
    pub name: String,
    pub kind: &'static str,
    /// starting | running | exited | failed | observed (seen, not supervised)
    pub collector_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub status_changed_at: Option<DateTime<Utc>>,
    /// Records received from this collector in this daemon run.
    pub events_received: u64,
    pub last_event_at: Option<DateTime<Utc>>,
}

fn source_kind(collectors: &Mutex<HashMap<String, CollectorCounters>>, name: &str) -> &'static str {
    collectors
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(name)
        .map_or("runtime", |c| c.kind)
}

impl AppState {
    /// Memory-only state with the default privacy policy. Used by tests and
    /// kept for API compatibility.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(source_rx: LogReceiver, capacity: usize) -> Arc<Self> {
        Self::with_limits(source_rx, capacity, tailflow_core::query::DEFAULT_MAX_BYTES)
    }

    /// Memory-only state with explicit ring bounds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_limits(source_rx: LogReceiver, capacity: usize, max_bytes: usize) -> Arc<Self> {
        let policy = Arc::new(
            CapturePolicy::from_config(&PrivacyConfig::default())
                .expect("the default privacy policy is valid"),
        );
        Self::with_pipeline(
            source_rx,
            RingLimits {
                records: capacity,
                bytes: max_bytes,
            },
            Pipeline {
                filter: Filter::none(),
                policy,
                storage: StorageSlot::Disabled,
                run_key: "memory".into(),
                shutdown: CancellationToken::new(),
                agent_ingest: crate::agent_ingest::disabled("memory-only state"),
                auth: Arc::new(crate::auth::Auth::generate()),
            },
        )
        .0
    }

    /// Spawn the single ingestion fan-out. Returns the state and the task,
    /// which finishes once every source has dropped its sender.
    pub fn with_pipeline(
        source_rx: LogReceiver,
        limits: RingLimits,
        pipeline: Pipeline,
    ) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let (tx, _) = broadcast::channel(tailflow_core::BUS_CAPACITY);
        let Pipeline {
            filter,
            policy,
            storage,
            run_key,
            shutdown,
            agent_ingest,
            auth,
        } = pipeline;
        let state = Arc::new(AppState {
            tx: tx.clone(),
            store: Arc::new(LogStore::with_limits(limits.records, limits.bytes)),
            storage,
            policy: policy.clone(),
            ingest: Arc::new(IngestStats::default()),
            shutdown,
            agent_ingest,
            auth,
            instance_id: format!("{}-{}", std::process::id(), Utc::now().timestamp_micros()),
            sources: Mutex::new(HashMap::new()),
            collectors: Arc::new(Mutex::new(HashMap::new())),
        });

        let store = state.store.clone();
        let ingest = state.ingest.clone();
        let collectors = state.collectors.clone();
        let storage = state.storage.storage().cloned();
        let mut source_rx = source_rx;
        let handle = tokio::spawn(async move {
            let redacts = policy.mode.redacts();
            let mut lines = LineRedactor::new(policy.redactor());
            loop {
                match source_rx.recv().await {
                    Ok(mut record) => {
                        if !filter.matches(&record) {
                            continue;
                        }
                        ingest.received.fetch_add(1, Ordering::Relaxed);
                        // Redact once, here, before the ring, SSE, legacy
                        // endpoints, MCP, and disk can see the line.
                        if redacts {
                            if let Some((clean, _)) =
                                lines.redact_line(&record.source, &record.payload)
                            {
                                record.payload = clean;
                                ingest.redacted.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        let kind = {
                            let mut map = collectors.lock().unwrap_or_else(|p| p.into_inner());
                            // Look up before inserting: `entry()` would clone
                            // the source name for every event.
                            if map.len() < MAX_COLLECTOR_ENTRIES
                                && !map.contains_key(&record.source)
                            {
                                map.insert(record.source.clone(), CollectorCounters::default());
                            }
                            match map.get_mut(&record.source) {
                                Some(entry) => {
                                    entry.received += 1;
                                    entry.last_event_at = Some(record.timestamp);
                                    if entry.kind.is_empty() {
                                        entry.kind = "runtime";
                                    }
                                    entry.kind
                                }
                                None => "runtime",
                            }
                        };
                        let seq_record = store.push(record);
                        if let Some(storage) = &storage {
                            let event = runtime_event(&seq_record, &run_key, kind);
                            let _outcome_is_counted_in_storage_health = storage.submit(event);
                        }
                        // No subscribers is normal (no dashboard open).
                        let _no_subscribers = tx.send(seq_record);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let now = Utc::now();
                        ingest.dropped.fetch_add(n, Ordering::Relaxed);
                        ingest
                            .last_drop_at_us
                            .store(now.timestamp_micros(), Ordering::Relaxed);
                        tracing::warn!(dropped = n, "ingestion fan-out lagged; records lost");
                        if let Some(storage) = &storage {
                            storage.record_upstream_loss(n, now, now);
                        }
                        // Make the loss visible in the live stream itself, so
                        // the dashboard, `/api/query`, and agents all see it.
                        let marker = store.push(LogRecord {
                            timestamp: now,
                            source: SYSTEM_SOURCE.into(),
                            level: LogLevel::Warn,
                            payload: format!(
                                "[tailflow] {n} log records were dropped: ingestion fell behind \
                                 (bus capacity {}). History around this point is incomplete.",
                                tailflow_core::BUS_CAPACITY
                            ),
                        });
                        let _no_subscribers = tx.send(marker);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        (state, handle)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn register_source(&self, name: &str) {
        self.register_source_kind(name, "runtime");
    }

    pub fn register_source_kind(&self, name: &str, kind: &'static str) {
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                name.to_string(),
                RuntimeSource {
                    status: "starting",
                    kind,
                    detail: None,
                    changed_at: Utc::now(),
                },
            );
        self.collectors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(name.to_string())
            .or_default()
            .kind = kind;
    }

    pub fn mark_source_running(&self, name: &str) {
        self.update_source(name, "running", None);
        self.collector_event(name, "system.collector.started", LogLevel::Info, None);
    }

    pub fn mark_source_exited(&self, name: &str, error: Option<String>) {
        let status = if error.is_some() { "failed" } else { "exited" };
        match &error {
            Some(e) => self.collector_event(
                name,
                "system.collector.error",
                LogLevel::Error,
                Some(format!("collector failed: {e}")),
            ),
            None => self.collector_event(name, "system.collector.stopped", LogLevel::Info, None),
        }
        self.update_source(name, status, error);
    }

    fn collector_event(
        &self,
        name: &str,
        event_type: &str,
        severity: LogLevel,
        message: Option<String>,
    ) {
        let Some(storage) = self.storage.storage() else {
            return;
        };
        let kind = source_kind(&self.collectors, name);
        let mut event = ObservabilityEvent::new(
            format!(
                "{}:collector:{name}:{}",
                storage.run_key(),
                Utc::now().timestamp_micros()
            ),
            event_type,
            Provenance::SystemObserved,
            kind,
            tailflow_core::event::bounded_id(name),
            Utc::now(),
        );
        event.severity = severity;
        event.message = message;
        let _outcome_is_counted_in_storage_health = storage.submit(event);
    }

    fn update_source(&self, name: &str, status: &'static str, detail: Option<String>) {
        let mut sources = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        let kind = sources.get(name).map_or("runtime", |s| s.kind);
        sources.insert(
            name.to_string(),
            RuntimeSource {
                status,
                kind,
                detail,
                changed_at: Utc::now(),
            },
        );
    }

    pub fn source_views(&self) -> Vec<SourceView> {
        let mut stats: HashMap<String, SourceStat> = self
            .store
            .sources()
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        let registered = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        let mut views = Vec::new();

        for (name, runtime) in registered.iter() {
            let stat = stats.remove(name);
            views.push(build_view(name.clone(), runtime.clone(), stat));
        }
        // Dynamically discovered sources such as individual Docker containers
        // have been observed, but the store alone cannot prove they are still
        // live. The registered Docker supervisor carries the live status.
        for (name, stat) in stats {
            views.push(build_view(
                name,
                RuntimeSource {
                    status: "observed",
                    kind: "runtime",
                    detail: None,
                    changed_at: stat.last_seen.unwrap_or_else(Utc::now),
                },
                Some(stat),
            ));
        }
        views.sort_by(|a, b| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then(b.errors.cmp(&a.errors))
                .then(a.name.cmp(&b.name))
        });
        views
    }

    pub fn collector_views(&self) -> Vec<CollectorView> {
        let registered = self
            .sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let counters = self
            .collectors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let mut names: Vec<&String> = registered.keys().chain(counters.keys()).collect();
        names.sort();
        names.dedup();
        let mut out: Vec<CollectorView> = names
            .into_iter()
            .map(|name| {
                let r = registered.get(name);
                let c = counters.get(name);
                CollectorView {
                    name: name.clone(),
                    kind: r.map(|r| r.kind).or(c.map(|c| c.kind)).unwrap_or("runtime"),
                    collector_status: r.map_or("observed", |r| r.status),
                    detail: r.and_then(|r| r.detail.clone()),
                    status_changed_at: r.map(|r| r.changed_at),
                    events_received: c.map_or(0, |c| c.received),
                    last_event_at: c.and_then(|c| c.last_event_at),
                }
            })
            .collect();
        out.sort_by(|a, b| {
            status_rank(a.collector_status)
                .cmp(&status_rank(b.collector_status))
                .then(a.name.cmp(&b.name))
        });
        out
    }
}

/// The persisted twin of a live record. Copies the payload once: the live
/// record stays shared by the ring and subscribers, while storage needs its
/// own copy to redact, truncate, and queue independently.
fn runtime_event(record: &SeqRecord, run_key: &str, kind: &str) -> ObservabilityEvent {
    let kind = if record.source == SYSTEM_SOURCE {
        "daemon"
    } else {
        kind
    };
    ObservabilityEvent::from_log_line(
        record.timestamp,
        // A label longer than the id bound (or with control characters)
        // would fail validation; keep it storable.
        tailflow_core::event::bounded_id(&record.source),
        record.level,
        record.payload.clone(),
        format!("{run_key}:{}", record.seq),
        kind,
        Some(record.seq),
    )
}

fn build_view(name: String, runtime: RuntimeSource, stat: Option<SourceStat>) -> SourceView {
    SourceView {
        name,
        status: runtime.status,
        detail: runtime.detail,
        total: stat.as_ref().map_or(0, |s| s.total),
        errors: stat.as_ref().map_or(0, |s| s.errors),
        warns: stat.as_ref().map_or(0, |s| s.warns),
        last_seen: stat.as_ref().and_then(|s| s.last_seen),
        last_line: stat.and_then(|s| s.last_line),
        status_changed_at: runtime.changed_at,
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "failed" => 0,
        "exited" => 1,
        "starting" => 2,
        "running" => 3,
        _ => 4,
    }
}
