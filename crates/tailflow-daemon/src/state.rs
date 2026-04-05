use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tailflow_core::{LogReceiver, LogRecord, LogSender};
use tokio::sync::broadcast;

const RING_SIZE: usize = 500;

/// Shared state for all HTTP handlers.
pub struct AppState {
    /// Subscribe to the live stream by calling `tx.subscribe()`.
    pub tx: LogSender,
    /// Rolling buffer of the last RING_SIZE records (for `/api/records`).
    pub ring: Mutex<VecDeque<LogRecord>>,
}

impl AppState {
    /// Create shared state and start the fan-out task.
    pub fn new(mut source_rx: LogReceiver) -> Arc<Self> {
        let (tx, _) = broadcast::channel(tailflow_core::BUS_CAPACITY);
        let state = Arc::new(AppState {
            tx: tx.clone(),
            ring: Mutex::new(VecDeque::with_capacity(RING_SIZE)),
        });
        let state2 = state.clone();

        tokio::spawn(async move {
            loop {
                match source_rx.recv().await {
                    Ok(record) => {
                        {
                            let mut buf = state2.ring.lock().unwrap_or_else(|p| p.into_inner());
                            if buf.len() >= RING_SIZE {
                                buf.pop_front(); // O(1) vs Vec::remove(0) O(n)
                            }
                            buf.push_back(record.clone());
                        }
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
