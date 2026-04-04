pub mod config;
pub mod ingestion;
pub mod processor;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// The canonical log record that flows through TailFlow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    pub source: String,
    pub level: LogLevel,
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Unknown,
}

impl LogLevel {
    /// Attempt to detect level from raw log text.
    pub fn detect(text: &str) -> Self {
        let lower = text.to_lowercase();
        if lower.contains("error") || lower.contains("err ") || lower.contains("fatal") {
            LogLevel::Error
        } else if lower.contains("warn") {
            LogLevel::Warn
        } else if lower.contains("debug") {
            LogLevel::Debug
        } else if lower.contains("trace") {
            LogLevel::Trace
        } else if lower.contains("info") {
            LogLevel::Info
        } else {
            LogLevel::Unknown
        }
    }
}

/// Shared broadcast bus capacity (number of buffered records).
pub const BUS_CAPACITY: usize = 4096;

pub type LogSender = broadcast::Sender<LogRecord>;
pub type LogReceiver = broadcast::Receiver<LogRecord>;

pub fn new_bus() -> (LogSender, LogReceiver) {
    broadcast::channel(BUS_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LogLevel::detect ─────────────────────────────────────────────────────

    #[test]
    fn detect_error_keyword() {
        assert_eq!(
            LogLevel::detect("ERROR: connection refused"),
            LogLevel::Error
        );
        assert_eq!(LogLevel::detect("error: timeout"), LogLevel::Error);
        assert_eq!(LogLevel::detect("FATAL: out of memory"), LogLevel::Error);
        assert_eq!(LogLevel::detect("something err happened"), LogLevel::Error);
    }

    #[test]
    fn detect_warn_keyword() {
        assert_eq!(LogLevel::detect("WARN: high memory"), LogLevel::Warn);
        assert_eq!(LogLevel::detect("warning: deprecated"), LogLevel::Warn);
    }

    #[test]
    fn detect_info_keyword() {
        assert_eq!(LogLevel::detect("INFO: server started"), LogLevel::Info);
        assert_eq!(
            LogLevel::detect("[info] listening on :8080"),
            LogLevel::Info
        );
    }

    #[test]
    fn detect_debug_keyword() {
        assert_eq!(LogLevel::detect("DEBUG: cache miss"), LogLevel::Debug);
        assert_eq!(LogLevel::detect("[debug] processing"), LogLevel::Debug);
    }

    #[test]
    fn detect_trace_keyword() {
        assert_eq!(LogLevel::detect("TRACE: entering fn"), LogLevel::Trace);
    }

    #[test]
    fn detect_unknown_for_plain_output() {
        assert_eq!(
            LogLevel::detect("server started on port 3000"),
            LogLevel::Unknown
        );
        assert_eq!(LogLevel::detect("compiled successfully"), LogLevel::Unknown);
    }

    #[test]
    fn detect_is_case_insensitive() {
        assert_eq!(LogLevel::detect("Error: bad"), LogLevel::Error);
        assert_eq!(LogLevel::detect("ERROR: bad"), LogLevel::Error);
        assert_eq!(LogLevel::detect("error: bad"), LogLevel::Error);
    }

    #[test]
    fn detect_error_takes_priority_over_warn() {
        // "error" beats "warn" when both appear
        assert_eq!(LogLevel::detect("error/warn mixed"), LogLevel::Error);
    }
}
