//! End-to-end regressions for M1.5 bounded line framing.
//!
//! M1 read collector output with `Lines`/`read_line`: an invalid UTF-8 byte
//! ended a process's capture silently (or failed a file collector), and a line
//! without a newline was buffered without limit. These run the real collectors.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant};
use tailflow_core::{
    ingestion::{file::FileSource, lines, process::ProcessSource, Source},
    new_bus, LogReceiver,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Cancels the collector even when an assertion panics first. Without it a
/// failing test hangs instead of failing: the runtime waits for the file
/// collector's blocking loop, which only stops on cancellation.
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

async fn collect_until(rx: &mut LogReceiver, want: &str, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(r)) => {
                let done = r.payload == want;
                out.push(r.payload);
                if done {
                    break;
                }
            }
            _ => break,
        }
    }
    out
}

// POSIX shell command (`;`, printf, /dev/zero); Windows runs commands via cmd.exe.
#[cfg(unix)]
#[tokio::test]
async fn process_capture_survives_invalid_utf8() {
    let (tx, mut rx) = new_bus();
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let src = ProcessSource::new("p", r"printf 'before\n\377\376 bad\nafter\n'; sleep 5");
    let token = shutdown.clone();
    let task = tokio::spawn(async move { Box::new(src).run(tx, token).await });

    let got = collect_until(&mut rx, "after", Duration::from_secs(5)).await;
    shutdown.cancel();
    let _ = task.await;
    assert!(got.contains(&"before".to_string()), "{got:?}");
    assert!(got.contains(&"\u{fffd}\u{fffd} bad".to_string()), "{got:?}");
    assert!(
        got.contains(&"after".to_string()),
        "capture must continue after an invalid byte: {got:?}"
    );
}

// POSIX shell command (`;`, printf, /dev/zero); Windows runs commands via cmd.exe.
#[cfg(unix)]
#[tokio::test]
async fn process_huge_line_is_cut_and_marked() {
    let (tx, mut rx) = new_bus();
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    // 1 MiB without a newline, then a normal line.
    let src = ProcessSource::new(
        "p",
        "head -c 1048576 /dev/zero | tr '\\000' x; printf '\\nnext\\n'; sleep 5",
    );
    let token = shutdown.clone();
    let task = tokio::spawn(async move { Box::new(src).run(tx, token).await });

    let got = collect_until(&mut rx, "next", Duration::from_secs(10)).await;
    shutdown.cancel();
    let _ = task.await;
    let big = got
        .iter()
        .find(|p| p.starts_with("xxxx"))
        .expect("the oversized line is delivered");
    assert!(
        big.len() <= lines::MAX_LINE_BYTES + 64,
        "kept {} bytes",
        big.len()
    );
    assert!(
        big.ends_with(&lines::truncation_marker(1_048_576)),
        "marker missing"
    );
    assert_eq!(got.last().map(String::as_str), Some("next"));
}

#[tokio::test]
async fn file_collector_survives_invalid_utf8() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("app.log");
    std::fs::write(&path, "").expect("create");
    let (tx, mut rx) = new_bus();
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let token = shutdown.clone();
    let src = FileSource::new(path.clone());
    let task = tokio::spawn(async move { Box::new(src).run(tx, token).await });
    wait_until_tailing(&path, &mut rx).await;

    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open")
        .write_all(b"\xffbad\ngood\n")
        .expect("write");
    let got = collect_until(&mut rx, "good", Duration::from_secs(5)).await;
    assert_eq!(got, ["\u{fffd}bad", "good"]);

    // Let the fallback poll back off to its 2 s ceiling, then stop: the
    // shutdown waker must end the loop without waiting for that poll.
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let t = Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("file collector stops")
        .expect("join")
        .expect("collector did not fail");
    assert!(
        t.elapsed() < Duration::from_millis(1_000),
        "shutdown waited for the idle poll: {:?}",
        t.elapsed()
    );
}
