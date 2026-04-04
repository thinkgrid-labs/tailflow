use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::Result;
use chrono::Utc;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tracing::{info, warn};

pub struct ProcessSource {
    label: String,
    /// Shell command string executed via `sh -c`
    cmd: String,
}

impl ProcessSource {
    pub fn new(label: impl Into<String>, cmd: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            cmd: cmd.into(),
        }
    }
}

#[async_trait::async_trait]
impl Source for ProcessSource {
    fn name(&self) -> &str {
        &self.label
    }

    async fn run(self: Box<Self>, tx: LogSender) -> Result<()> {
        info!(label = %self.label, cmd = %self.cmd, "spawning process");

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&self.cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let tx_out = tx.clone();
        let label_out = self.label.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let record = LogRecord {
                    timestamp: Utc::now(),
                    source: label_out.clone(),
                    level: LogLevel::detect(&line),
                    payload: line,
                };
                if tx_out.send(record).is_err() {
                    break;
                }
            }
        });

        let tx_err = tx.clone();
        let label_err = self.label.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let record = LogRecord {
                    timestamp: Utc::now(),
                    source: label_err.clone(),
                    level: LogLevel::detect(&line),
                    payload: line,
                };
                if tx_err.send(record).is_err() {
                    break;
                }
            }
        });

        let status = child.wait().await?;
        stdout_task.await.ok();
        stderr_task.await.ok();

        if !status.success() {
            warn!(label = %self.label, code = ?status.code(), "process exited non-zero");
        } else {
            info!(label = %self.label, "process exited");
        }

        Ok(())
    }
}
