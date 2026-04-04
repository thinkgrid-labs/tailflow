use crate::ingestion::{
    docker::DockerSource, file::FileSource, process::ProcessSource, stdin::StdinSource, Source,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level `tailflow.toml` structure.
#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub sources: SourcesConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct SourcesConfig {
    /// `docker = true` — tail all running containers
    #[serde(default)]
    pub docker: bool,

    /// `stdin = "label"` — treat piped input as this named source
    pub stdin: Option<String>,

    /// `[[sources.file]]` entries
    #[serde(default)]
    pub file: Vec<FileEntry>,

    /// `[[sources.process]]` entries
    #[serde(default)]
    pub process: Vec<ProcessEntry>,
}

#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProcessEntry {
    pub cmd: String,
    pub label: String,
}

impl Config {
    /// Load from `tailflow.toml` in the given directory (or any parent).
    pub fn find_and_load(start: &Path) -> Result<Option<Self>> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join("tailflow.toml");
            if candidate.exists() {
                return Ok(Some(Self::load(&candidate)?));
            }
            if !dir.pop() {
                return Ok(None);
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
    }

    /// Resolve config into a list of ingestion sources.
    /// Async because DockerSource::discover() calls the Docker API.
    pub async fn into_sources(self) -> Result<Vec<Box<dyn Source>>> {
        let mut sources: Vec<Box<dyn Source>> = Vec::new();

        if self.sources.docker {
            let containers = DockerSource::discover().await?;
            for c in containers {
                sources.push(Box::new(c));
            }
        }

        for entry in self.sources.file {
            let label = entry
                .label
                .unwrap_or_else(|| {
                    entry
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string()
                });
            let _ = label; // FileSource uses the filename as name()
            sources.push(Box::new(FileSource::new(entry.path)));
        }

        for entry in self.sources.process {
            sources.push(Box::new(ProcessSource::new(entry.label, entry.cmd)));
        }

        if let Some(label) = self.sources.stdin {
            sources.push(Box::new(StdinSource::new(label)));
        }

        Ok(sources)
    }
}
