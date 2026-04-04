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
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait::async_trait]
impl Source for FileSource {
    fn name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
    }

    async fn run(self: Box<Self>, tx: LogSender) -> Result<()> {
        let path = self.path.clone();
        let source_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        // Run blocking file I/O on the blocking thread pool.
        task::spawn_blocking(move || -> Result<()> {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("cannot open {}", path.display()))?;
            let mut reader = BufReader::new(file);

            // Seek to end so we only emit new lines.
            reader.seek(SeekFrom::End(0))?;

            let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
            let mut watcher = RecommendedWatcher::new(event_tx, notify::Config::default())?;
            watcher.watch(&path, RecursiveMode::NonRecursive)?;

            info!(path = %path.display(), "watching file");

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
