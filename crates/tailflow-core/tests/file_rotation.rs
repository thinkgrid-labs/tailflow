use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{Duration, Instant};
use tailflow_core::{
    ingestion::{file::FileSource, Source},
    new_bus, LogReceiver,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Wait until the collector is tailing `path`. A file collector starts at
/// the end of the file, so a line written before it opened the file is never
/// seen; a fixed sleep races that on a loaded host. Append numbered markers
/// until the newest one is delivered (earlier ones precede it in the file,
/// so none can arrive later and leak into the test).
async fn wait_until_tailing(path: &std::path::Path, rx: &mut LogReceiver) {
    let deadline = Instant::now() + Duration::from_secs(20);
    for n in 0u32.. {
        assert!(
            Instant::now() < deadline,
            "the file collector never started tailing"
        );
        let marker = format!("ready-{n}");
        OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open for append")
            .write_all(format!("{marker}\n").as_bytes())
            .expect("append marker");
        let wait_until = Instant::now() + Duration::from_millis(250);
        while let Ok(Ok(r)) = tokio::time::timeout(
            wait_until.saturating_duration_since(Instant::now()),
            rx.recv(),
        )
        .await
        {
            if r.payload == marker {
                return;
            }
        }
    }
}

async fn next_payload(rx: &mut tailflow_core::LogReceiver) -> String {
    tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .expect("timed out waiting for tailed line")
        .expect("source bus closed")
        .payload
}

#[tokio::test]
async fn file_source_survives_rotation_and_truncation() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("app.log");
    fs::write(&path, "old\n").unwrap();
    let (tx, mut rx) = new_bus();
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let task_shutdown = shutdown.clone();
    let source = FileSource::new(path.clone());
    let task = tokio::spawn(async move { Box::new(source).run(tx, task_shutdown).await });
    wait_until_tailing(&path, &mut rx).await;

    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"one\n")
        .unwrap();
    assert_eq!(next_payload(&mut rx).await, "one");

    fs::rename(&path, dir.path().join("app.log.1")).unwrap();
    fs::write(&path, "two\n").unwrap();
    assert_eq!(next_payload(&mut rx).await, "two");

    fs::write(&path, "thr").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), rx.recv())
            .await
            .is_err(),
        "a partial filesystem write must not become a partial log record"
    );
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"ee\n")
        .unwrap();
    assert_eq!(next_payload(&mut rx).await, "three");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("file watcher did not stop")
        .unwrap()
        .unwrap();
}
