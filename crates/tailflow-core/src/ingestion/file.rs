use super::{
    lines::{read_bounded_line, LineRead, MAX_LINE_BYTES},
    Source,
};
use crate::{LogLevel, LogRecord, LogSender};
use anyhow::{Context, Result};
use chrono::Utc;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(not(unix))]
use std::time::SystemTime;
use std::{
    fs::{File, Metadata},
    io::{BufReader, Read, Seek, SeekFrom},
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
    fn kind(&self) -> &'static str {
        "file"
    }

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
        let (wake_tx, wake_rx) = mpsc::channel::<Wake>();

        // Wake the blocking loop the moment shutdown is requested, so stopping
        // never waits for the next poll.
        let shutdown_waker = {
            let tx = wake_tx.clone();
            let token = shutdown.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                let _receiver_gone = tx.send(Wake::Shutdown);
            })
        };

        let watched = task::spawn_blocking(move || -> Result<()> {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let fs_tx = wake_tx;
            let mut watcher = RecommendedWatcher::new(
                move |event: notify::Result<Event>| {
                    if event.as_ref().is_ok_and(|e| !may_change_content(e)) {
                        return;
                    }
                    let _receiver_gone = fs_tx.send(Wake::Fs(event));
                },
                notify::Config::default(),
            )?;
            watcher.watch(parent, RecursiveMode::NonRecursive)?;

            // Existing files start at EOF, matching `tail -f`. A file created
            // after startup is read from byte zero because all of it is new.
            let mut open = open_file(&path, true)?;
            info!(path = %path.display(), source = %source_name, "watching file");

            // Change notifications drive reads. The poll is only a fallback
            // for filesystems that coalesce a rename and create into an event
            // that does not name our path, so it backs off while the file is
            // quiet instead of waking (and re-opening the file) 4× a second.
            let mut poll = POLL_MIN;
            while !shutdown.is_cancelled() {
                match wake_rx.recv_timeout(poll) {
                    Ok(Wake::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Wake::Fs(Ok(_))) => {
                        read_available(&path, &source_name, &tx, &mut open)?;
                        poll = POLL_MIN;
                    }
                    Ok(Wake::Fs(Err(e))) => warn!(err = %e, "file watch error"),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        poll = if read_available(&path, &source_name, &tx, &mut open)? {
                            POLL_MIN
                        } else {
                            poll.saturating_mul(2).min(POLL_MAX)
                        };
                    }
                }
            }
            Ok(())
        })
        .await;
        shutdown_waker.abort();
        watched??;
        Ok(())
    }
}

/// Fallback poll interval right after activity, and its idle ceiling.
const POLL_MIN: Duration = Duration::from_millis(250);
const POLL_MAX: Duration = Duration::from_secs(2);

/// Whether a filesystem event can mean new or replaced content.
///
/// Access events (open, read, close-without-write) cannot. On Linux, inotify
/// reports them, and every pass here opens the file to verify its checkpoint:
/// reacting to those opens fed back into itself and kept one core at 100 %
/// for as long as a file was tailed (M1 and earlier; found in M1.5).
fn may_change_content(event: &Event) -> bool {
    use notify::event::{AccessKind, AccessMode};
    match event.kind {
        // A writer closing the file is real activity — and this collector
        // only ever opens files read-only, so it cannot cause one itself.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

/// What woke the blocking watch loop.
enum Wake {
    Fs(notify::Result<Event>),
    Shutdown,
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

/// Emit every complete line appended since the last call. Returns whether
/// anything changed (lines emitted, or the file appeared, rotated, or was
/// truncated), which resets the fallback poll interval.
fn read_available(
    path: &Path,
    source_name: &str,
    tx: &LogSender,
    open: &mut Option<OpenFile>,
) -> Result<bool> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let was_open = open.is_some();
            *open = None;
            return Ok(was_open);
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
        return Ok(should_reopen);
    };
    let mut changed = should_reopen;
    loop {
        let line_start = file.position;
        // Bounded and lossy: an oversized line is cut (and marked) and a
        // non-UTF-8 byte is replaced, instead of failing the collector.
        let payload = match read_bounded_line(&mut file.reader, MAX_LINE_BYTES)? {
            // M1 removed every trailing `\r` from tailed lines; keep that.
            LineRead::Line(mut payload) => {
                let end = payload.trim_end_matches('\r').len();
                payload.truncate(end);
                payload
            }
            LineRead::Eof => break,
            LineRead::Partial { .. } => {
                // A write notification may arrive between truncate and the
                // final newline. Rewind so the next pass emits one complete
                // record instead of splitting it into arbitrary filesystem
                // write chunks.
                file.reader.seek(SeekFrom::Start(line_start))?;
                break;
            }
        };
        file.position = file.reader.stream_position()?;
        changed = true;
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
    }
    if changed {
        if let Some(checkpoint) = read_checkpoint(path, file.position)? {
            file.checkpoint = checkpoint;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind};

    #[test]
    fn access_events_never_wake_the_reader() {
        let ev = |kind| Event::new(kind);
        assert!(!may_change_content(&ev(EventKind::Access(
            AccessKind::Open(AccessMode::Any)
        ))));
        assert!(!may_change_content(&ev(EventKind::Access(
            AccessKind::Close(AccessMode::Read)
        ))));
        assert!(!may_change_content(&ev(EventKind::Access(
            AccessKind::Read
        ))));
        // Only a writer produces close-after-write; keep it.
        assert!(may_change_content(&ev(EventKind::Access(
            AccessKind::Close(AccessMode::Write)
        ))));
        assert!(may_change_content(&ev(EventKind::Modify(ModifyKind::Any))));
        assert!(may_change_content(&ev(EventKind::Create(CreateKind::File))));
        assert!(may_change_content(&ev(EventKind::Remove(
            notify::event::RemoveKind::Any
        ))));
        assert!(may_change_content(&ev(EventKind::Any)));
    }
}
