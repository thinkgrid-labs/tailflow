use crate::{LogReceiver, LogRecord};
use regex::Regex;
use tokio::sync::broadcast;

pub struct Filter {
    /// Regex matched against `record.payload`.
    grep: Option<Regex>,
    /// Substring matched against `record.source`.
    source: Option<String>,
}

impl Filter {
    pub fn none() -> Self {
        Self {
            grep: None,
            source: None,
        }
    }

    /// Build a filter that matches records whose payload matches `pattern`.
    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            grep: Some(Regex::new(pattern)?),
            source: None,
        })
    }

    /// Add (or replace) a source substring filter.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Returns `true` if the record passes all active filters.
    pub fn matches(&self, record: &LogRecord) -> bool {
        if let Some(src) = &self.source {
            if !record.source.contains(src.as_str()) {
                return false;
            }
        }
        match &self.grep {
            None => true,
            Some(re) => re.is_match(&record.payload),
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
    fn filter_source_matches_source_name() {
        let f = Filter::none().with_source("api");
        assert!(f.matches(&record("api", "anything")));
        assert!(!f.matches(&record("worker", "anything")));
    }

    #[test]
    fn filter_source_is_substring_match() {
        let f = Filter::none().with_source("web");
        assert!(f.matches(&record("web-server", "started")));
        assert!(f.matches(&record("web-worker", "started")));
        assert!(!f.matches(&record("api", "started")));
    }

    #[test]
    fn filter_grep_only_matches_payload_not_source() {
        let f = Filter::regex("api").unwrap();
        // payload matches
        assert!(f.matches(&record("worker", "calling api endpoint")));
        // source name alone does not match
        assert!(!f.matches(&record("api", "server started")));
    }

    #[test]
    fn filter_grep_and_source_both_must_match() {
        let f = Filter::regex("error").unwrap().with_source("web");
        assert!(f.matches(&record("web-server", "error: timeout")));
        assert!(!f.matches(&record("api", "error: timeout"))); // source mismatch
        assert!(!f.matches(&record("web-server", "all good"))); // grep mismatch
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
