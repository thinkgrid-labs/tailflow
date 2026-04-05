/// Integration tests for the processor module (Filter + filtered_bus).
use chrono::Utc;
use tailflow_core::{
    new_bus,
    processor::{filtered_bus, Filter},
    LogLevel, LogRecord,
};
use tokio::time::{timeout, Duration};

fn record(source: &str, payload: &str) -> LogRecord {
    LogRecord {
        timestamp: Utc::now(),
        source: source.to_string(),
        level: LogLevel::Info,
        payload: payload.to_string(),
    }
}

/// Keep `_tx` alive long enough for the spawned filter task to process records
/// before the source channel closes.
macro_rules! recv_with_live_tx {
    ($out:expr, $tx:expr) => {
        timeout(Duration::from_secs(1), $out.recv())
            .await
            .expect("test timed out waiting for filtered record")
            .expect("filtered bus closed unexpectedly")
    };
}

#[tokio::test]
async fn filtered_bus_passes_matching_records() {
    let (tx, rx) = new_bus();
    let mut out = filtered_bus(rx, Filter::regex("error").unwrap());

    tx.send(record("api", "INFO: server started")).unwrap();
    tx.send(record("api", "error: connection refused")).unwrap();

    // Keep tx alive so the spawned task can process both records before the
    // source channel closes.
    let r = recv_with_live_tx!(out, tx);

    assert_eq!(r.payload, "error: connection refused");
    drop(tx);
}

#[tokio::test]
async fn filtered_bus_blocks_non_matching_records() {
    let (tx, rx) = new_bus();
    let mut out = filtered_bus(rx, Filter::regex("ONLY_THIS").unwrap());

    tx.send(record("api", "nothing relevant here")).unwrap();
    drop(tx);

    // Channel should close with no matching records delivered
    let result = timeout(Duration::from_millis(300), out.recv()).await;
    match result {
        // Closed after draining — correct
        Ok(Err(_)) => {}
        // Timeout is also acceptable: filter task may not have exited yet
        Err(_) => {}
        Ok(Ok(r)) => panic!("unexpected record passed filter: {:?}", r.payload),
    }
}

#[tokio::test]
async fn filter_none_passes_all_records() {
    let (tx, rx) = new_bus();
    let mut out = filtered_bus(rx, Filter::none());

    tx.send(record("svc", "first")).unwrap();
    tx.send(record("svc", "second")).unwrap();

    let r1 = recv_with_live_tx!(out, tx);
    let r2 = recv_with_live_tx!(out, tx);

    assert_eq!(r1.payload, "first");
    assert_eq!(r2.payload, "second");
    drop(tx);
}

#[tokio::test]
async fn filtered_bus_matches_on_source_name() {
    let (tx, rx) = new_bus();
    let mut out = filtered_bus(rx, Filter::none().with_source("frontend"));

    tx.send(record("backend", "request handled")).unwrap();
    tx.send(record("frontend", "compiled in 1.2s")).unwrap();

    let r = recv_with_live_tx!(out, tx);

    assert_eq!(r.source, "frontend");
    drop(tx);
}

#[tokio::test]
async fn filtered_bus_passes_multiple_matching_records() {
    let (tx, rx) = new_bus();
    let mut out = filtered_bus(rx, Filter::regex("(?i)error").unwrap());

    tx.send(record("api", "no match")).unwrap();
    tx.send(record("api", "error one")).unwrap();
    tx.send(record("api", "no match")).unwrap();
    tx.send(record("api", "ERROR two")).unwrap();

    let r1 = recv_with_live_tx!(out, tx);
    let r2 = recv_with_live_tx!(out, tx);

    assert_eq!(r1.payload, "error one");
    assert_eq!(r2.payload, "ERROR two");
    drop(tx);
}
