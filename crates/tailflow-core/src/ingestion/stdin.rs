use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::Result;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, BufReader};
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

    async fn run(self: Box<Self>, tx: LogSender) -> Result<()> {
        info!(source = %self.label, "reading from stdin");
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();

        while let Some(line) = lines.next_line().await? {
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
