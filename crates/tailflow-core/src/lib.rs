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
