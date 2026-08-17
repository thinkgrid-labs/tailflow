use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::Result;
use bollard::{container::LogsOptions, Docker};
use chrono::Utc;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct DockerSource {
    container_id: String,
    container_name: String,
}

/// Supervises all running containers for the lifetime of TailFlow. Container
/// IDs are reconciled continuously so `docker compose up --build` replacements
/// and containers started after the daemon are captured automatically.
pub struct DockerAllSource;

impl DockerAllSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DockerAllSource {
    fn default() -> Self {
        Self::new()
    }
}

struct ManagedContainer {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl DockerSource {
    pub fn new(container_id: impl Into<String>, container_name: impl Into<String>) -> Self {
        Self {
            container_id: container_id.into(),
            container_name: container_name.into(),
        }
    }

    /// Discover all running containers and return one DockerSource per container.
    pub async fn discover() -> Result<Vec<DockerSource>> {
        let docker = Docker::connect_with_local_defaults()?;
        let containers = docker
            .list_containers::<String>(Some(bollard::container::ListContainersOptions {
                all: false,
                ..Default::default()
            }))
            .await?;

        let sources = containers
            .into_iter()
            .filter_map(|c| {
                let id = c.id?;
                let name = c
                    .names
                    .and_then(|n| n.into_iter().next())
                    .unwrap_or_else(|| id.chars().take(12).collect());
                Some(DockerSource::new(id, name.trim_start_matches('/')))
            })
            .collect();

        Ok(sources)
    }
}

#[async_trait::async_trait]
impl Source for DockerSource {
    fn name(&self) -> &str {
        &self.container_name
    }

    async fn run(self: Box<Self>, tx: LogSender, shutdown: CancellationToken) -> Result<()> {
        let docker = Docker::connect_with_local_defaults()?;
        info!(container = %self.container_name, "starting docker log tail");

        let opts = LogsOptions::<String> {
            follow: true,
            stdout: true,
            stderr: true,
            tail: "50".to_string(),
            ..Default::default()
        };

        let mut stream = docker.logs(&self.container_id, Some(opts));

        loop {
            let output = tokio::select! {
                _ = shutdown.cancelled() => break,
                output = stream.next() => output,
            };
            let Some(output) = output else { break };
            match output {
                Ok(log_output) => {
                    let payload = log_output.to_string();
                    let payload = payload.trim_end_matches('\n').to_string();
                    if payload.is_empty() {
                        continue;
                    }
                    let record = LogRecord {
                        timestamp: Utc::now(),
                        source: self.container_name.clone(),
                        level: LogLevel::detect(&payload),
                        payload,
                    };
                    if tx.send(record).is_err() {
                        break; // bus dropped — shut down
                    }
                }
                Err(e) => {
                    warn!(container = %self.container_name, err = %e, "docker log stream error");
                    break;
                }
            }
        }

        info!(container = %self.container_name, "docker log tail ended");
        Ok(())
    }
}

#[async_trait::async_trait]
impl Source for DockerAllSource {
    fn name(&self) -> &str {
        "docker"
    }

    async fn run(self: Box<Self>, tx: LogSender, shutdown: CancellationToken) -> Result<()> {
        let mut managed: HashMap<String, ManagedContainer> = HashMap::new();

        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match DockerSource::discover().await {
                Ok(discovered) => {
                    let active: HashSet<String> =
                        discovered.iter().map(|s| s.container_id.clone()).collect();

                    let removed: Vec<String> = managed
                        .keys()
                        .filter(|id| !active.contains(*id))
                        .cloned()
                        .collect();
                    for id in removed {
                        if let Some(entry) = managed.remove(&id) {
                            entry.cancel.cancel();
                            let _ = entry.task.await;
                        }
                    }
                    managed.retain(|_, entry| !entry.task.is_finished());

                    for source in discovered {
                        if managed.contains_key(&source.container_id) {
                            continue;
                        }
                        let id = source.container_id.clone();
                        let name = source.container_name.clone();
                        let cancel = shutdown.child_token();
                        let child_cancel = cancel.clone();
                        let child_tx = tx.clone();
                        let _ = tx.send(LogRecord {
                            timestamp: Utc::now(),
                            source: name.clone(),
                            level: LogLevel::Info,
                            payload: "[tailflow] attached to Docker container".into(),
                        });
                        let task = tokio::spawn(async move {
                            if let Err(e) =
                                Box::new(source).run(child_tx.clone(), child_cancel).await
                            {
                                let _ = child_tx.send(LogRecord {
                                    timestamp: Utc::now(),
                                    source: name,
                                    level: LogLevel::Error,
                                    payload: format!("[tailflow] Docker log stream failed: {e}"),
                                });
                            }
                        });
                        managed.insert(id, ManagedContainer { cancel, task });
                    }
                }
                Err(e) => {
                    warn!(err = %e, "Docker discovery failed; will retry");
                    let _ = tx.send(LogRecord {
                        timestamp: Utc::now(),
                        source: "docker".into(),
                        level: LogLevel::Error,
                        payload: format!("[tailflow] Docker discovery failed: {e}"),
                    });
                }
            }

            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }

        for (_, entry) in managed {
            entry.cancel.cancel();
            let _ = entry.task.await;
        }
        Ok(())
    }
}
