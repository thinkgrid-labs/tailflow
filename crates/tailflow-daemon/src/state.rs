use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tailflow_core::{
    query::{LogStore, SeqRecord, SourceStat},
    LogReceiver,
};
use tokio::sync::broadcast;

pub struct AppState {
    pub tx: broadcast::Sender<SeqRecord>,
    pub store: Arc<LogStore>,
    sources: Mutex<HashMap<String, RuntimeSource>>,
}

#[derive(Clone)]
struct RuntimeSource {
    status: &'static str,
    detail: Option<String>,
    changed_at: DateTime<Utc>,
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

impl AppState {
    pub fn new(source_rx: LogReceiver, capacity: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(tailflow_core::BUS_CAPACITY);
        let state = Arc::new(AppState {
            tx: tx.clone(),
            store: Arc::new(LogStore::new(capacity)),
            sources: Mutex::new(HashMap::new()),
        });

        let store = state.store.clone();
        let mut source_rx = source_rx;
        tokio::spawn(async move {
            loop {
                match source_rx.recv().await {
                    Ok(record) => {
                        let record = store.push(record);
                        let _ = tx.send(record);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "state fan-out lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        state
    }

    pub fn register_source(&self, name: &str) {
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                name.to_string(),
                RuntimeSource {
                    status: "starting",
                    detail: None,
                    changed_at: Utc::now(),
                },
            );
    }

    pub fn mark_source_running(&self, name: &str) {
        self.update_source(name, "running", None);
    }

    pub fn mark_source_exited(&self, name: &str, error: Option<String>) {
        let status = if error.is_some() { "failed" } else { "exited" };
        self.update_source(name, status, error);
    }

    fn update_source(&self, name: &str, status: &'static str, detail: Option<String>) {
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                name.to_string(),
                RuntimeSource {
                    status,
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
