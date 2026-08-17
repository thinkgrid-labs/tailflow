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
        if let Some(level) = structured_level(text) {
            return level;
        }
        let lower = text.to_lowercase();
        let tokens: Vec<&str> = text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|token| !token.is_empty())
            .collect();
        let words: Vec<String> = tokens
            .iter()
            .map(|token| token.to_ascii_lowercase())
            .collect();
        let explicitly_clean = [
            "0 error",
            "0 failure",
            "no error",
            "no failure",
            "without error",
            "error_count 0",
            "error_count=0",
            "errors: 0",
            "errors=0",
            "error-free",
            "failure-free",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        let failure_word = words.iter().any(|word| {
            matches!(
                word.as_str(),
                "err"
                    | "error"
                    | "errors"
                    | "fatal"
                    | "panic"
                    | "panicked"
                    | "exception"
                    | "failed"
                    | "failure"
                    | "traceback"
            )
        }) || tokens
            .iter()
            .any(|token| token.ends_with("Error") || token.ends_with("Exception"))
            || lower.contains("segmentation fault")
            || lower.contains("unhandled rejection")
            || lower.contains("uncaught exception");

        if failure_word && !explicitly_clean {
            LogLevel::Error
        } else if words.iter().any(|word| word.starts_with("warn")) {
            LogLevel::Warn
        } else if words.iter().any(|word| word == "debug") {
            LogLevel::Debug
        } else if words.iter().any(|word| word == "trace") {
            LogLevel::Trace
        } else if words.iter().any(|word| word == "info") {
            LogLevel::Info
        } else {
            LogLevel::Unknown
        }
    }
}

fn structured_level(text: &str) -> Option<LogLevel> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let object = value.as_object()?;
    for key in [
        "level",
        "severity",
        "severity_text",
        "log_level",
        "loglevel",
        "log.level",
    ] {
        if let Some(level) = object
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .and_then(|(_, value)| level_value(value))
        {
            return Some(level);
        }
    }
    object
        .get("log")?
        .as_object()?
        .get("level")
        .and_then(level_value)
}

fn level_value(value: &serde_json::Value) -> Option<LogLevel> {
    if let Some(level) = value.as_str() {
        return match level.trim().to_ascii_lowercase().as_str() {
            "critical" | "crit" | "emergency" | "emerg" | "alert" => Some(LogLevel::Error),
            other => LogLevel::parse(other),
        };
    }
    // Syslog severities: 0–3 error, 4 warning, 5–6 informational, 7 debug.
    value.as_u64().and_then(|n| match n {
        0..=3 => Some(LogLevel::Error),
        4 => Some(LogLevel::Warn),
        5..=6 => Some(LogLevel::Info),
        7 => Some(LogLevel::Debug),
        _ => None,
    })
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
        assert_eq!(LogLevel::detect("Failed to compile"), LogLevel::Error);
        assert_eq!(
            LogLevel::detect("thread panicked at src/main.rs"),
            LogLevel::Error
        );
        assert_eq!(LogLevel::detect("NullPointerException"), LogLevel::Error);
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
        assert_eq!(
            LogLevel::detect("Build finished with 0 errors"),
            LogLevel::Unknown
        );
        assert_eq!(LogLevel::detect("no failures detected"), LogLevel::Unknown);
        assert_eq!(LogLevel::detect("error-free build"), LogLevel::Unknown);
        assert_eq!(
            LogLevel::detect("terror level increased"),
            LogLevel::Unknown
        );
    }

    #[test]
    fn detect_prefers_structured_json_level() {
        assert_eq!(
            LogLevel::detect(r#"{"level":"warn","message":"error budget healthy"}"#),
            LogLevel::Warn
        );
        assert_eq!(
            LogLevel::detect(r#"{"severity_text":"ERROR","message":"boom"}"#),
            LogLevel::Error
        );
        assert_eq!(
            LogLevel::detect(r#"{"log":{"level":"debug"},"message":"x"}"#),
            LogLevel::Debug
        );
        assert_eq!(
            LogLevel::detect(r#"{"log.level":"error","message":"boom"}"#),
            LogLevel::Error
        );
        assert_eq!(
            LogLevel::detect(r#"{"severity":3,"message":"x"}"#),
            LogLevel::Error
        );
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
