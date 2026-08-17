use super::Source;
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::{Context, Result};
use chrono::Utc;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(not(unix))]
use std::time::SystemTime;
use std::{
    fs::{File, Metadata},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};
use tokio::task;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct FileSource {
    path: PathBuf,
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

    async fn run(self: Box<Self>, tx: LogSender, shutdown: CancellationToken) -> Result<()> {
        let path = self.path.clone();
        let source_name = self.name().to_string();

        task::spawn_blocking(move || -> Result<()> {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
            let mut watcher = RecommendedWatcher::new(event_tx, notify::Config::default())?;
            watcher.watch(parent, RecursiveMode::NonRecursive)?;

            // Existing files start at EOF, matching `tail -f`. A file created
            // after startup is read from byte zero because all of it is new.
            let mut open = open_file(&path, true)?;
            info!(path = %path.display(), source = %source_name, "watching file");

            while !shutdown.is_cancelled() {
                match event_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(Ok(_)) => read_available(&path, &source_name, &tx, &mut open)?,
                    Ok(Err(e)) => warn!(err = %e, "file watch error"),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // Polling also covers filesystems that coalesce a rename
                        // and create into an event that does not name our path.
                        read_available(&path, &source_name, &tx, &mut open)?;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        })
        .await??;
        Ok(())
    }
}

struct OpenFile {
    reader: BufReader<File>,
    identity: FileIdentity,
    position: u64,
    checkpoint: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    primary: u64,
    secondary: u64,
}

#[cfg(unix)]
fn file_identity(meta: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        primary: meta.dev(),
        secondary: meta.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(meta: &Metadata) -> FileIdentity {
    let created = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    FileIdentity {
        primary: created,
        secondary: 0,
    }
}

fn open_file(path: &Path, seek_to_end: bool) -> Result<Option<OpenFile>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let meta = file.metadata()?;
    let identity = file_identity(&meta);
    let mut reader = BufReader::new(file);
    let position = if seek_to_end {
        reader.seek(SeekFrom::End(0))?
    } else {
        reader.seek(SeekFrom::Start(0))?
    };
    let checkpoint = read_checkpoint(path, position)?.unwrap_or_default();
    Ok(Some(OpenFile {
        reader,
        identity,
        position,
        checkpoint,
    }))
}

/// Remember a short byte window immediately before the current offset. File
/// length alone cannot detect a truncate-and-rewrite that grows past the old
/// offset before the watcher runs; the checkpoint distinguishes that case
/// from a normal append.
fn read_checkpoint(path: &Path, position: u64) -> Result<Option<Vec<u8>>> {
    const CHECKPOINT_BYTES: u64 = 64;
    let start = position.saturating_sub(CHECKPOINT_BYTES);
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (position - start) as usize];
    match file.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(bytes)),
        // The file changed between metadata/checkpoint reads. Treat that as a
        // reopen signal; it is a normal rotation race, not a source failure.
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_available(
    path: &Path,
    source_name: &str,
    tx: &LogSender,
    open: &mut Option<OpenFile>,
) -> Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            *open = None;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let identity = file_identity(&meta);
    let should_reopen = match open.as_ref() {
        None => true,
        Some(file) if file.identity != identity || meta.len() < file.position => true,
        Some(file) => read_checkpoint(path, file.position)?.as_ref() != Some(&file.checkpoint),
    };
    if should_reopen {
        *open = open_file(path, false)?;
    }

    let Some(file) = open.as_mut() else {
        return Ok(());
    };
    let mut line = String::new();
    loop {
        let line_start = file.position;
        if file.reader.read_line(&mut line)? == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // A write notification may arrive between truncate and the final
            // newline. Rewind so the next pass emits one complete record
            // instead of splitting it into arbitrary filesystem write chunks.
            file.reader.seek(SeekFrom::Start(line_start))?;
            break;
        }
        file.position = file.reader.stream_position()?;
        let payload = line.trim_end_matches(['\n', '\r']).to_string();
        if !payload.is_empty()
            && tx
                .send(LogRecord {
                    timestamp: Utc::now(),
                    source: source_name.to_string(),
                    level: LogLevel::detect(&payload),
                    payload,
                })
                .is_err()
        {
            break;
        }
        line.clear();
    }
    if let Some(checkpoint) = read_checkpoint(path, file.position)? {
        file.checkpoint = checkpoint;
    }
    Ok(())
}
