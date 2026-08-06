use std::sync::Arc;
use tailflow_core::{query::LogStore, LogReceiver, LogSender};
use tokio::sync::broadcast;

/// Shared state for all HTTP handlers.
pub struct AppState {
    /// Subscribe to the live stream by calling `tx.subscribe()`.
    pub tx: LogSender,
    /// Retrospective buffer backing `/api/records` and the agent endpoints.
    pub store: Arc<LogStore>,
}

impl AppState {
    /// Create shared state and start the fan-out task.
    pub fn new(source_rx: LogReceiver, capacity: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(tailflow_core::BUS_CAPACITY);
        let state = Arc::new(AppState {
            tx: tx.clone(),
            store: Arc::new(LogStore::new(capacity)),
        });

        let store = state.store.clone();
        let mut source_rx = source_rx;
        tokio::spawn(async move {
            loop {
                match source_rx.recv().await {
                    Ok(record) => {
                        store.push(record.clone());
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
}
