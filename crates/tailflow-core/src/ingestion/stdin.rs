use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::Result;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct StdinSource {
    label: String,
}

impl StdinSource {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

#[async_trait::async_trait]
impl Source for StdinSource {
    fn name(&self) -> &str {
        &self.label
    }

    async fn run(self: Box<Self>, tx: LogSender, shutdown: CancellationToken) -> Result<()> {
        info!(source = %self.label, "reading from stdin");
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();

        loop {
            let line = tokio::select! {
                _ = shutdown.cancelled() => break,
                line = lines.next_line() => line?,
            };
            let Some(line) = line else { break };
            let payload = line.trim_end_matches('\n').to_string();
            if payload.is_empty() {
                continue;
            }
            let record = LogRecord {
                timestamp: Utc::now(),
                source: self.label.clone(),
                level: LogLevel::detect(&payload),
                payload,
            };
            if tx.send(record).is_err() {
                break;
            }
        }

        Ok(())
    }
}
