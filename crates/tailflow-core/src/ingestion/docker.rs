use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::Result;
use bollard::{
    container::LogsOptions,
    Docker,
};
use chrono::Utc;
use futures_util::StreamExt;
use tracing::{info, warn};

pub struct DockerSource {
    container_id: String,
    container_name: String,
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

    async fn run(self: Box<Self>, tx: LogSender) -> Result<()> {
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

        while let Some(output) = stream.next().await {
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
