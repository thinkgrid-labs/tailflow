use crate::{LogReceiver, LogRecord};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LogLevel, LogRecord};
    use chrono::Utc;

    fn record(source: &str, payload: &str) -> LogRecord {
        LogRecord {
            timestamp: Utc::now(),
            source: source.to_string(),
            level: LogLevel::Info,
            payload: payload.to_string(),
        }
    }

    // ── Filter::matches ───────────────────────────────────────────────────────

    #[test]
    fn filter_none_matches_everything() {
        let f = Filter::none();
        assert!(f.matches(&record("api", "server started")));
        assert!(f.matches(&record("worker", "queue depth 100")));
    }

    #[test]
    fn filter_regex_matches_payload() {
        let f = Filter::regex("error|ERROR").unwrap();
        assert!(f.matches(&record("api", "ERROR: connection refused")));
        assert!(f.matches(&record("api", "something error here")));
        assert!(!f.matches(&record("api", "server started")));
    }

    #[test]
    fn filter_regex_matches_source_name() {
        let f = Filter::regex("^api$").unwrap();
        assert!(f.matches(&record("api", "anything")));
        assert!(!f.matches(&record("worker", "anything")));
    }

    #[test]
    fn filter_regex_is_case_sensitive_by_default() {
        let f = Filter::regex("ERROR").unwrap();
        assert!(f.matches(&record("x", "ERROR: bad")));
        assert!(!f.matches(&record("x", "error: bad")));
    }

    #[test]
    fn filter_invalid_regex_returns_err() {
        assert!(Filter::regex("[[[not a regex").is_err());
    }
}
