use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::Duration;
use tailflow_core::{
    ingestion::{file::FileSource, Source},
    new_bus,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
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
    tokio::time::sleep(Duration::from_millis(300)).await;

    fs::write(&path, "old\none\n").unwrap();
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
