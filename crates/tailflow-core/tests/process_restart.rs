use std::time::{Duration, Instant};
use tailflow_core::{
    config::RestartPolicy,
    ingestion::{process::ProcessSource, Source},
    new_bus,
};

/// Collect up to `limit` records from `rx` within `timeout`.
async fn collect(
    mut rx: tailflow_core::LogReceiver,
    limit: usize,
    timeout: Duration,
) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    let mut payloads = Vec::new();
    while payloads.len() < limit && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(r)) => payloads.push(r.payload),
            _ => break,
        }
    }
    payloads
}

// ── RestartPolicy::Never ──────────────────────────────────────────────────────

#[tokio::test]
async fn never_policy_does_not_restart_on_failure() {
    let (tx, rx) = new_bus();
    let src =
        ProcessSource::new("test", "echo hello && exit 1").with_restart(RestartPolicy::Never, 50);

    tokio::spawn(async move { Box::new(src).run(tx).await });

    let payloads = collect(rx, 10, Duration::from_secs(3)).await;
    // Should get exactly one "hello", no restart synthetic record.
    assert_eq!(payloads.iter().filter(|p| p.as_str() == "hello").count(), 1);
    assert!(
        !payloads.iter().any(|p| p.contains("[tailflow]")),
        "unexpected restart record with Never policy"
    );
}

// ── RestartPolicy::OnFailure ──────────────────────────────────────────────────

#[tokio::test]
async fn on_failure_restarts_after_non_zero_exit() {
    let (tx, rx) = new_bus();
    // Exits non-zero — should trigger a restart synthetic record.
    let src = ProcessSource::new("test", "exit 1").with_restart(RestartPolicy::OnFailure, 50);

    tokio::spawn(async move { Box::new(src).run(tx).await });

    let payloads = collect(rx, 5, Duration::from_secs(3)).await;
    assert!(
        payloads
            .iter()
            .any(|p| p.contains("[tailflow]") && p.contains("restarting")),
        "expected restart record after non-zero exit, got: {payloads:?}"
    );
}

#[tokio::test]
async fn on_failure_does_not_restart_after_clean_exit() {
    let (tx, rx) = new_bus();
    // Exits zero — should NOT restart.
    let src = ProcessSource::new("test", "echo done").with_restart(RestartPolicy::OnFailure, 50);

    tokio::spawn(async move { Box::new(src).run(tx).await });

    let payloads = collect(rx, 10, Duration::from_secs(2)).await;
    assert!(
        !payloads.iter().any(|p| p.contains("[tailflow]")),
        "unexpected restart record after clean exit"
    );
}

// ── RestartPolicy::Always ─────────────────────────────────────────────────────

#[tokio::test]
async fn always_policy_restarts_after_clean_exit() {
    let (tx, rx) = new_bus();
    // Exits zero — Always policy should still restart.
    let src = ProcessSource::new("test", "echo hi").with_restart(RestartPolicy::Always, 50);

    tokio::spawn(async move { Box::new(src).run(tx).await });

    let payloads = collect(rx, 10, Duration::from_secs(3)).await;
    assert!(
        payloads
            .iter()
            .any(|p| p.contains("[tailflow]") && p.contains("restarting")),
        "expected restart record with Always policy, got: {payloads:?}"
    );
}

// ── Backoff ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn restart_delay_is_respected() {
    let (tx, rx) = new_bus();
    // 200 ms initial delay so we can measure it.
    let src = ProcessSource::new("test", "exit 1").with_restart(RestartPolicy::OnFailure, 200);

    tokio::spawn(async move { Box::new(src).run(tx).await });

    let start = Instant::now();
    // Wait for the first restart synthetic record.
    let payloads = collect(rx, 2, Duration::from_secs(3)).await;
    let elapsed = start.elapsed();

    assert!(
        payloads.iter().any(|p| p.contains("[tailflow]")),
        "expected restart record"
    );
    // At least the configured delay elapsed before the record appeared.
    assert!(
        elapsed >= Duration::from_millis(150),
        "restart fired too fast ({elapsed:?})"
    );
}
