use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::{Context, Result};
use chrono::Utc;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::PathBuf,
    sync::mpsc,
};
use tokio::task;
use tracing::{info, warn};

pub struct FileSource {
    path: PathBuf,
    /// Optional override for the source name shown in the UI.
    /// Falls back to the filename when `None`.
    label: Option<String>,
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            label: None,
        }
    }

    pub fn with_label(path: impl Into<PathBuf>, label: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            label: Some(label.into()),
        }
    }
}

#[async_trait::async_trait]
impl Source for FileSource {
    fn name(&self) -> &str {
        if let Some(l) = &self.label {
            return l.as_str();
        }
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
    }

    async fn run(self: Box<Self>, tx: LogSender) -> Result<()> {
        let path = self.path.clone();
        let source_name = self.name().to_string();

        task::spawn_blocking(move || -> Result<()> {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("cannot open {}", path.display()))?;
            let mut reader = BufReader::new(file);

            // Seek to end — only emit newly appended lines.
            reader.seek(SeekFrom::End(0))?;

            let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
            let mut watcher = RecommendedWatcher::new(event_tx, notify::Config::default())?;
            watcher.watch(&path, RecursiveMode::NonRecursive)?;

            info!(path = %path.display(), source = %source_name, "watching file");

            for res in event_rx {
                match res {
                    Ok(event) if matches!(event.kind, EventKind::Modify(_)) => {
                        let mut line = String::new();
                        while reader.read_line(&mut line)? > 0 {
                            let payload = line.trim_end_matches('\n').to_string();
                            if !payload.is_empty() {
                                let record = LogRecord {
                                    timestamp: Utc::now(),
                                    source: source_name.clone(),
                                    level: LogLevel::detect(&payload),
                                    payload,
                                };
                                if tx.send(record).is_err() {
                                    return Ok(());
                                }
                            }
                            line.clear();
                        }
                    }
                    Err(e) => {
                        warn!(err = %e, "file watch error");
                        break;
                    }
                    _ => {}
                }
            }

            Ok(())
        })
        .await??;

        Ok(())
    }
}
