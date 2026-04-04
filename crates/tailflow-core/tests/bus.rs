/// Integration tests for the broadcast bus (`new_bus`).
///
/// These run in a separate compilation unit so they exercise the public API
/// exactly as downstream crates would.
use chrono::Utc;
use tailflow_core::{new_bus, LogLevel, LogRecord};

fn make(source: &str, payload: &str) -> LogRecord {
    LogRecord {
        timestamp: Utc::now(),
        source: source.to_string(),
        level: LogLevel::Info,
        payload: payload.to_string(),
    }
}

#[tokio::test]
async fn single_receiver_gets_sent_record() {
    let (tx, mut rx) = new_bus();
    tx.send(make("api", "hello")).unwrap();

    let r = rx.recv().await.unwrap();
    assert_eq!(r.source, "api");
    assert_eq!(r.payload, "hello");
}

#[tokio::test]
async fn multiple_subscribers_each_receive_record() {
    let (tx, mut rx1) = new_bus();
    let mut rx2 = tx.subscribe();

    tx.send(make("svc", "broadcast")).unwrap();

    let r1 = rx1.recv().await.unwrap();
    let r2 = rx2.recv().await.unwrap();
    assert_eq!(r1.payload, "broadcast");
    assert_eq!(r2.payload, "broadcast");
}

#[tokio::test]
async fn records_preserve_level() {
    let (tx, mut rx) = new_bus();

    let mut rec = make("api", "ERROR: boom");
    rec.level = LogLevel::Error;
    tx.send(rec).unwrap();

    let r = rx.recv().await.unwrap();
    assert_eq!(r.level, LogLevel::Error);
}

#[tokio::test]
async fn receiver_closed_when_all_senders_dropped() {
    let (tx, mut rx) = new_bus();
    drop(tx);

    let result = rx.recv().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn bus_handles_rapid_burst() {
    let (tx, mut rx) = new_bus();
    let n = 200u32;

    for i in 0..n {
        tx.send(make("burst", &format!("msg-{i}"))).unwrap();
    }
    drop(tx);

    let mut count = 0u32;
    while rx.recv().await.is_ok() {
        count += 1;
    }
    assert_eq!(count, n);
}
