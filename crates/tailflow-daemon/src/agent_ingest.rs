//! The daemon side of agent capture: one blocking thread that moves spool
//! records into storage.
//!
//! Event-driven: a non-recursive watcher on the spool inbox wakes the thread
//! when a hook renames a record into it; a 30 s rescan is the fallback for
//! filesystems that drop notifications. Wakes are filtered to *records that
//! exist in the inbox*, so the thread's own reads, claims (renames out of
//! the inbox) and Linux inotify access events never wake it (the M1.5
//! busy-loop lesson). After a wake it waits [`DEBOUNCE`] so a burst of hook
//! calls is ingested — and committed — together.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tailflow_core::agent::spool::Spool;
use tailflow_storage::ingest::{IngestReport, SpoolIngestor};
use tailflow_storage::Storage;
use tokio_util::sync::CancellationToken;

const FALLBACK_RESCAN: Duration = Duration::from_secs(30);
const DEBOUNCE: Duration = Duration::from_millis(100);
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// What the ingest thread has done, for `/api/v1/agents/status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestStatus {
    pub enabled: bool,
    pub spool_dir: Option<String>,
    pub passes: u64,
    pub files_ingested: u64,
    pub events_submitted: u64,
    pub events_dropped: u64,
    pub excluded: u64,
    pub rejected: u64,
    pub corrupt_files: u64,
    pub overflow: u64,
    pub last_pass_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    /// Why ingestion is off, when it is.
    pub disabled_reason: Option<String>,
}

impl IngestStatus {
    fn add(&mut self, r: &IngestReport) {
        self.passes += 1;
        self.files_ingested += r.files as u64;
        self.events_submitted += r.events_submitted as u64;
        self.events_dropped += r.events_dropped as u64;
        self.excluded += r.excluded;
        self.rejected += r.rejected;
        self.corrupt_files += r.corrupt_files as u64;
        self.overflow += r.overflow;
        self.last_pass_at = Some(chrono::Utc::now());
        if let Some(e) = r.errors.last() {
            self.last_error = Some(e.clone());
        }
    }
}

pub type SharedStatus = Arc<Mutex<IngestStatus>>;

pub fn disabled(reason: &str) -> SharedStatus {
    Arc::new(Mutex::new(IngestStatus {
        disabled_reason: Some(reason.to_string()),
        ..IngestStatus::default()
    }))
}

enum Wake {
    Fs,
    Stop,
}

fn is_inbox_record(dir: &Path, p: &Path) -> bool {
    p.parent() == Some(dir)
        && p.extension().is_some_and(|e| e == "json")
        && p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.') && n != "overflow.json")
        && p.exists()
}

/// A running ingest thread.
pub struct Handle {
    thread: std::thread::JoinHandle<()>,
    stop: mpsc::Sender<Wake>,
}

impl Handle {
    /// Stop after the current pass and release unfinished claims. Blocking.
    pub fn stop(self) {
        let _thread_may_be_gone = self.stop.send(Wake::Stop);
        if self.thread.join().is_err() {
            tracing::warn!("agent ingest thread panicked");
        }
    }
}

/// Start ingesting `spool` into `storage`. The watcher is best effort: if
/// it cannot be created the thread still rescans every 30 s.
pub fn spawn(
    storage: Storage,
    spool: Spool,
    shutdown: &CancellationToken,
    status: SharedStatus,
) -> std::io::Result<Handle> {
    tailflow_core::paths::create_private_dir(spool.dir())?;
    let (tx, rx) = mpsc::channel::<Wake>();
    {
        let mut s = status.lock().unwrap_or_else(|p| p.into_inner());
        s.enabled = true;
        s.spool_dir = Some(spool.dir().display().to_string());
    }
    // Wake the thread as soon as the daemon starts stopping.
    let stop_tx = tx.clone();
    let token = shutdown.clone();
    tokio::spawn(async move {
        token.cancelled().await;
        let _thread_may_be_gone = stop_tx.send(Wake::Stop);
    });
    let dir: PathBuf = spool.dir().to_path_buf();
    let fs_tx = tx.clone();
    let watch_dir = dir.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if ev.paths.iter().any(|p| is_inbox_record(&watch_dir, p)) {
                let _thread_may_be_gone = fs_tx.send(Wake::Fs);
            }
        }
    })
    .and_then(|mut w| {
        use notify::Watcher;
        w.watch(&dir, notify::RecursiveMode::NonRecursive)?;
        Ok(w)
    });
    let watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(err = %e, "agent spool watcher unavailable; rescanning every 30 s");
            None
        }
    };
    let ingestor = SpoolIngestor::new(spool, storage.run_key().to_string());
    let thread = std::thread::Builder::new()
        .name("tailflow-agent-ingest".into())
        .spawn(move || {
            let _keep_watcher_alive = watcher;
            let pass = |status: &SharedStatus| {
                let r = ingestor.drain(&storage, DRAIN_BUDGET);
                if r.files > 0 || !r.errors.is_empty() || r.overflow > 0 {
                    tracing::debug!(
                        files = r.files,
                        events = r.events_submitted,
                        "agent spool ingested"
                    );
                }
                for e in &r.errors {
                    tracing::warn!(err = %e, "agent spool");
                }
                status.lock().unwrap_or_else(|p| p.into_inner()).add(&r);
                r.more
            };
            // Whatever accumulated while no daemon was running.
            let mut more = pass(&status);
            loop {
                let wait = if more {
                    Duration::ZERO
                } else {
                    FALLBACK_RESCAN
                };
                match rx.recv_timeout(wait) {
                    Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Wake::Fs) => {
                        // Coalesce a burst, then drain everything at once.
                        std::thread::sleep(DEBOUNCE);
                        let mut stop = false;
                        while let Ok(w) = rx.try_recv() {
                            stop |= matches!(w, Wake::Stop);
                        }
                        if stop {
                            break;
                        }
                        more = pass(&status);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => more = pass(&status),
                }
            }
            ingestor.release();
        })?;
    Ok(Handle { thread, stop: tx })
}
