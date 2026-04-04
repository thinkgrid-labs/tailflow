use crate::{LogRecord, LogReceiver};
use regex::Regex;
use tokio::sync::broadcast;

pub struct Filter {
    pattern: Option<Regex>,
}

impl Filter {
    pub fn none() -> Self {
        Self { pattern: None }
    }

    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: Some(Regex::new(pattern)?),
        })
    }

    pub fn matches(&self, record: &LogRecord) -> bool {
        match &self.pattern {
            None => true,
            Some(re) => re.is_match(&record.payload) || re.is_match(&record.source),
        }
    }
}

/// Spawns a task that reads from `rx`, applies `filter`, and re-publishes
/// matching records on a new channel.  Returns the new receiver.
pub fn filtered_bus(mut rx: LogReceiver, filter: Filter) -> LogReceiver {
    let (tx, new_rx) = broadcast::channel::<LogRecord>(crate::BUS_CAPACITY);

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(record) => {
                    if filter.matches(&record) {
                        let _ = tx.send(record);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "filter bus lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    new_rx
}
