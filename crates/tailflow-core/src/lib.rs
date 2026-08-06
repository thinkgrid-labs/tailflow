pub mod config;
pub mod ingestion;
pub mod json;
pub mod processor;
pub mod query;

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
    /// Rank used for `>= min_level` comparisons.
    ///
    /// `Unknown` ranks *lowest* — plain output like `compiled successfully`
    /// carries no level marker, so it must not satisfy a `warn`-or-above
    /// query. Continuation lines of a multi-line stack trace also land here;
    /// [`crate::query::LogStore::summarize`] recovers those separately as
    /// trailing context rather than by level.
    pub fn severity(self) -> u8 {
        match self {
            LogLevel::Unknown => 0,
            LogLevel::Trace => 1,
            LogLevel::Debug => 2,
            LogLevel::Info => 3,
            LogLevel::Warn => 4,
            LogLevel::Error => 5,
        }
    }

    /// Parse a level name. Accepts common aliases and is case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "trace" | "trc" => Some(LogLevel::Trace),
            "debug" | "dbg" => Some(LogLevel::Debug),
            "info" | "inf" => Some(LogLevel::Info),
            "warn" | "warning" | "wrn" => Some(LogLevel::Warn),
            "error" | "err" | "fatal" => Some(LogLevel::Error),
            "unknown" | "any" | "all" => Some(LogLevel::Unknown),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Unknown => "unknown",
        }
    }

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
