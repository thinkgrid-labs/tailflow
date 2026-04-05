use super::Source;
use crate::{config::RestartPolicy, LogLevel, LogRecord, LogSender};
use anyhow::Result;
use chrono::Utc;
use std::{process::ExitStatus, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tracing::{info, warn};

pub struct ProcessSource {
    label: String,
    cmd: String,
    restart_policy: RestartPolicy,
    /// Initial restart delay in ms; doubles each attempt, capped at 30 s.
    restart_delay_ms: u64,
}

impl ProcessSource {
    pub fn new(label: impl Into<String>, cmd: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            cmd: cmd.into(),
            restart_policy: RestartPolicy::Never,
            restart_delay_ms: 1_000,
        }
    }

    pub fn with_restart(mut self, policy: RestartPolicy, delay_ms: u64) -> Self {
        self.restart_policy = policy;
        self.restart_delay_ms = delay_ms;
        self
    }

    /// Spawn the process once and stream its stdout/stderr into `tx`.
    /// Returns the process exit status.
    async fn run_once(&self, tx: &LogSender) -> Result<ExitStatus> {
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
        if let Err(e) = stdout_task.await {
            tracing::warn!(label = %self.label, err = ?e, "stdout reader task panicked");
        }
        if let Err(e) = stderr_task.await {
            tracing::warn!(label = %self.label, err = ?e, "stderr reader task panicked");
        }

        Ok(status)
    }
}

#[async_trait::async_trait]
impl Source for ProcessSource {
    fn name(&self) -> &str {
        &self.label
    }

    async fn run(self: Box<Self>, tx: LogSender) -> Result<()> {
        let mut attempt: u32 = 0;

        loop {
            info!(label = %self.label, cmd = %self.cmd, attempt, "spawning process");

            let status = self.run_once(&tx).await?;

            let should_restart = match self.restart_policy {
                RestartPolicy::Never => false,
                RestartPolicy::Always => true,
                RestartPolicy::OnFailure => !status.success(),
            };

            if !should_restart {
                if status.success() {
                    info!(label = %self.label, "process exited cleanly");
                } else {
                    warn!(label = %self.label, code = ?status.code(), "process exited non-zero");
                }
                break;
            }

            // Stop restarting if the bus has no receivers (daemon shutting down).
            if tx.receiver_count() == 0 {
                break;
            }

            // Exponential backoff: delay * 2^attempt, capped at 30 s.
            let delay_ms =
                (self.restart_delay_ms.saturating_mul(1u64 << attempt.min(5))).min(30_000);

            let exit_desc = status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());

            warn!(
                label = %self.label,
                exit = %exit_desc,
                delay_ms,
                attempt,
                "process crashed, scheduling restart"
            );

            // Emit a synthetic record so the restart appears in every consumer's stream.
            let _ = tx.send(LogRecord {
                timestamp: Utc::now(),
                source: self.label.clone(),
                level: LogLevel::Warn,
                payload: format!(
                    "[tailflow] process exited ({exit_desc}), restarting in {delay_ms} ms \
                     (attempt {attempt})"
                ),
            });

            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            attempt += 1;
        }

        Ok(())
    }
}
