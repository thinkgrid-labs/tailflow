//! Fault injection: TailFlow must never report complete history after
//! confirmed loss.

mod common;

use common::*;
use rusqlite::Connection;
use std::time::{Duration, Instant};
use tailflow_core::privacy::PrivacyConfig;
use tailflow_core::LogLevel;
use tailflow_storage::{EventQuery, LossReason, Storage, StorageError, Submitted, WriterState};

/// Hold the database write lock from another connection for `hold`.
fn hold_write_lock(path: &std::path::Path, hold: Duration) -> std::thread::JoinHandle<()> {
    let path = path.to_path_buf();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        let c = Connection::open(&path).unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(hold);
        c.execute_batch("COMMIT").unwrap();
    });
    ready_rx.recv().unwrap();
    h
}

#[test]
fn queue_overflow_is_counted_recorded_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut cfg = config(&path);
    cfg.queue_capacity = 256;
    let s = Storage::open(&cfg, opts(&PrivacyConfig::default())).unwrap();
    let _ = s.submit(log_event(0, "api", LogLevel::Info, "baseline"));
    s.flush(T).unwrap();

    // Stall the writer, then overflow its queue.
    let lock = hold_write_lock(&path, Duration::from_millis(1500));
    let mut dropped = 0;
    for i in 1..=2_000 {
        if let Submitted::Dropped(LossReason::QueueFull) =
            s.submit(log_event(i, "api", LogLevel::Info, "burst"))
        {
            dropped += 1;
        }
    }
    assert!(dropped > 0, "the queue must actually overflow in this test");
    let mid = s.health();
    assert!(mid.loss_detected);
    assert_eq!(mid.events_dropped.queue_full, dropped);
    // Before the gap row is even written, a query must not claim completeness.
    let p = s.query(&EventQuery::default()).unwrap();
    assert!(
        !p.completeness.complete,
        "pending loss must be visible immediately"
    );
    assert!(p.completeness.reasons.contains(&"pending_loss"));

    lock.join().unwrap();
    s.flush(T).unwrap();
    let h = s.health();
    assert_eq!(h.pending_loss_unrecorded, 0, "loss is now durable");
    assert_eq!(
        h.events_received,
        h.events_enqueued + h.events_dropped_total,
        "every received event is accounted for"
    );
    let p = s
        .query(&EventQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert!(!p.completeness.complete);
    assert!(p.completeness.reasons.contains(&"loss_in_range"));
    let gap = &p.completeness.gaps[0];
    assert_eq!(gap.reason, "queue_full");
    assert_eq!(gap.count, Some(dropped));
    assert!(
        p.events
            .iter()
            .any(|e| e.event.event_type == "system.events.dropped"),
        "loss is visible in the stream too"
    );

    // And the loss survives a restart.
    s.shutdown(T).unwrap();
    drop(s);
    let s = open(&path);
    let p = s
        .query(&EventQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert!(
        !p.completeness.complete,
        "loss must be durable across restarts"
    );
}

#[test]
fn lock_contention_is_retried_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    let _ = s.submit(log_event(0, "api", LogLevel::Info, "warm"));
    s.flush(T).unwrap();
    // Longer than the 5 s busy timeout: the first attempt fails with BUSY and
    // a retry succeeds once the lock is released.
    let lock = hold_write_lock(&path, Duration::from_millis(5_300));
    for i in 1..=100 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, "during lock"));
    }
    lock.join().unwrap();
    s.flush(T).unwrap();
    let h = s.health();
    assert!(h.storage_errors >= 1, "contention was observed: {h:?}");
    assert_eq!(h.events_dropped_total, 0, "and retried without loss");
    assert_eq!(h.events_persisted, 101);
    assert_eq!(
        h.writer_state,
        WriterState::Ok,
        "state recovers after success"
    );
    assert!(
        s.query(&EventQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap()
        .completeness
        .complete
    );
}

#[test]
fn disk_full_degrades_then_recovers_and_records_the_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut cfg = config(&path);
    cfg.full_text_search = false;
    // Seed some rows so that a later purge frees pages for reuse.
    {
        let s = Storage::open(&cfg, opts(&PrivacyConfig::default())).unwrap();
        for i in 0..400 {
            let _ = s.submit(log_event(i, "api", LogLevel::Info, &"seed ".repeat(200)));
        }
        s.flush(T).unwrap();
        s.shutdown(T).unwrap();
    }
    let pages: i64 = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    // The writer may not grow the file: the next insert that needs a new page
    // fails with SQLITE_FULL, exactly as on a full disk.
    let s = Storage::open_with_writer_hook(&cfg, opts(&PrivacyConfig::default()), move |c| {
        c.pragma_update(None, "max_page_count", pages + 1)?;
        Ok(())
    })
    .unwrap();
    for i in 0..300 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, &"full ".repeat(400)));
    }
    s.flush(T).unwrap();
    let h = s.health();
    assert!(h.storage_errors > 0, "{h:?}");
    assert!(h.events_dropped.storage_error > 0, "{h:?}");
    assert!(
        h.last_error.as_deref().unwrap_or("").contains("full"),
        "{h:?}"
    );
    assert!(
        h.pending_loss_unrecorded > 0,
        "cannot record the gap while full"
    );
    let p = s
        .query(&EventQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert!(
        !p.completeness.complete,
        "unrecorded loss must still be reported"
    );

    // Free space (as retention would), then the writer recovers and the loss
    // becomes durable.
    tailflow_storage::admin::purge(&path, tailflow_storage::admin::PurgeTarget::All).unwrap();
    let _ = s.submit(log_event(999, "api", LogLevel::Info, "after recovery"));
    s.flush(T).unwrap();
    let h = s.health();
    assert_eq!(h.writer_state, WriterState::Ok, "{h:?}");
    assert_eq!(h.pending_loss_unrecorded, 0);
    let p = s
        .query(&EventQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert!(p
        .events
        .iter()
        .any(|e| e.event.message.as_deref() == Some("after recovery")));
    assert!(!p.completeness.complete);
    assert!(p
        .completeness
        .gaps
        .iter()
        .any(|g| g.reason == "storage_error"));
}

#[test]
fn unclean_previous_run_is_recovered_as_a_gap_of_unknown_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    for i in 0..10 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, "before crash"));
    }
    s.flush(T).unwrap();
    s.shutdown(T).unwrap();
    drop(s);
    // Simulate `kill -9`: an open run row whose process no longer exists.
    {
        let c = Connection::open(&path).unwrap();
        c.execute(
            "INSERT INTO runs (run_key, started_at, heartbeat_at, version, pid, last_seq)
             VALUES ('dead', 1, strftime('%s','now') * 1000000, 'test', 2147483000, 10)",
            [],
        )
        .unwrap();
    }
    let s = open(&path);
    assert_eq!(s.recovered_runs().len(), 1);
    let p = s.query(&EventQuery::default()).unwrap();
    assert!(!p.completeness.complete);
    let gap = p
        .completeness
        .gaps
        .iter()
        .find(|g| g.reason == "unclean_shutdown")
        .unwrap();
    assert_eq!(
        gap.count, None,
        "an unclean exit loses an unknown number of events"
    );
    assert!(p
        .events
        .iter()
        .any(|e| e.event.event_type == "system.events.dropped"));
    // Recovery is idempotent: a second open finds nothing new.
    s.shutdown(T).unwrap();
    drop(s);
    let s = open(&path);
    assert!(s.recovered_runs().is_empty());
}

#[test]
fn a_live_concurrent_daemon_is_not_mistaken_for_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let a = open(&path);
    // A second daemon sharing the default DB path.
    let b = open(&path);
    assert!(b.recovered_runs().is_empty(), "the other run is alive");
    let _ = a.submit(log_event(1, "a", LogLevel::Info, "from a"));
    let _ = b.submit(log_event(1, "b", LogLevel::Info, "from b"));
    a.flush(T).unwrap();
    b.flush(T).unwrap();
    assert_eq!(a.query(&EventQuery::default()).unwrap().events.len(), 2);
}

#[test]
fn corrupt_and_unusable_paths_fail_without_touching_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    std::fs::write(&path, b"definitely not sqlite".repeat(100)).unwrap();
    let before = std::fs::read(&path).unwrap();
    let err = Storage::open(&config(&path), opts(&PrivacyConfig::default()))
        .err()
        .unwrap();
    assert!(matches!(err, StorageError::NotADatabase { .. }), "{err:?}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(err.to_string().contains("left untouched"));
    assert!(
        !err.public_message()
            .contains(&*dir.path().to_string_lossy()),
        "no paths in API errors"
    );

    let err = Storage::open(&config(dir.path()), opts(&PrivacyConfig::default()))
        .err()
        .unwrap();
    assert!(matches!(err, StorageError::CannotOpen { .. }), "{err:?}");
}

#[test]
fn submissions_after_shutdown_are_counted_as_loss() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    s.shutdown(T).unwrap();
    assert_eq!(
        s.submit(log_event(1, "api", LogLevel::Info, "late")),
        Submitted::Dropped(LossReason::WriterStopped)
    );
    assert_eq!(s.health().events_dropped.writer_stopped, 1);
    assert!(s.health().loss_detected);
}

#[test]
fn shutdown_drains_everything_queued() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut cfg = config(&path);
    cfg.batch_max_delay_ms = 2_000; // events sit in the batch
    let s = Storage::open(&cfg, opts(&PrivacyConfig::default())).unwrap();
    for i in 0..1_000 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, "queued"));
    }
    let started = Instant::now();
    s.shutdown(T).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(count_rows(&path), 1_000, "clean shutdown loses nothing");
    let c = Connection::open(&path).unwrap();
    let clean: i64 = c
        .query_row("SELECT clean FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(clean, 1);
}

// ── Regression tests for the M1 independent review ───────────────────────────

#[test]
fn review_r1_invalid_events_are_loss_not_silence() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let mut e = log_event(1, "api", LogLevel::Info, "x");
    e.source_id = "s".repeat(10_000); // fails validation
    assert_eq!(s.submit(e), Submitted::Invalid);
    let _ = s.submit(log_event(2, "api", LogLevel::Info, "ok"));
    s.flush(T).unwrap();
    let h = s.health();
    assert!(h.loss_detected, "{h:?}");
    assert_eq!(h.events_dropped.invalid, 1);
    let p = s.query(&EventQuery::default()).unwrap();
    assert!(!p.completeness.complete);
    assert!(p
        .completeness
        .gaps
        .iter()
        .any(|g| g.reason == "invalid_event"));
}

#[test]
fn review_r1_overlong_project_path_still_persists() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(&PrivacyConfig::default());
    o.project_id = Some(format!("/{}", "deep/".repeat(200)));
    let s = Storage::open(&config(&db_path(&dir)), o).unwrap();
    assert_eq!(
        s.submit(log_event(1, "api", LogLevel::Info, "x")),
        Submitted::Enqueued
    );
    s.flush(T).unwrap();
    assert_eq!(s.health().events_persisted, 1);
}

#[test]
fn review_r2_loss_during_the_final_commit_is_never_marked_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    // A short busy timeout makes the writer's final commit fail fast while
    // another connection holds the write lock through shutdown.
    let s = Storage::open_with_writer_hook(&config(&path), opts(&PrivacyConfig::default()), |c| {
        c.busy_timeout(Duration::from_millis(50))?;
        Ok(())
    })
    .unwrap();
    let _ = s.submit(log_event(0, "api", LogLevel::Info, "warm"));
    s.flush(T).unwrap();
    let lock = hold_write_lock(&path, Duration::from_millis(1_500));
    for i in 1..=50 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, "in flight at shutdown"));
    }
    s.shutdown(T).unwrap();
    lock.join().unwrap();
    assert!(s.health().events_dropped.storage_error >= 50);
    drop(s);
    let c = Connection::open(&path).unwrap();
    let clean: i64 = c
        .query_row("SELECT clean FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(clean, 0, "a run that lost events is never marked clean");
    drop(c);
    let s = open(&path);
    let p = s.query(&EventQuery::default()).unwrap();
    assert!(
        !p.completeness.complete,
        "the loss is durable: {:?}",
        p.completeness
    );
    assert!(p
        .completeness
        .gaps
        .iter()
        .any(|g| g.reason == "storage_error"));
}

#[test]
fn review_r6_unclean_gap_covers_events_written_by_other_daemons() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    for i in 0..20 {
        let _ = s.submit(log_event(i, "b", LogLevel::Info, "other daemon"));
    }
    s.flush(T).unwrap();
    s.shutdown(T).unwrap();
    drop(s);
    {
        // A crashed run whose last commit was seq 5, long before seq 20.
        let c = Connection::open(&path).unwrap();
        c.execute(
            "INSERT INTO runs (run_key, started_at, heartbeat_at, version, pid, last_seq)
             VALUES ('dead', 1, 1, 'test', 2147483000, 5)",
            [],
        )
        .unwrap();
    }
    let s = open(&path);
    // A `since`-style query over the recent range must still see the gap.
    let p = s
        .query(&EventQuery {
            after_seq: Some(15),
            ..Default::default()
        })
        .unwrap();
    assert!(!p.completeness.complete, "{:?}", p.completeness);
}

#[test]
fn review_r9_payload_pages_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let err = s
        .query(&EventQuery {
            include_payload: true,
            limit: 500,
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.kind(), "invalid_query");
    let big = "m".repeat(15 * 1024);
    for i in 0..1000 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, &big));
    }
    s.flush(T).unwrap();
    let p = s
        .query(&EventQuery {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert!(
        p.events.len() < 1000,
        "the byte budget stops the page early"
    );
    assert!(p.has_more);
    let total: usize = p
        .events
        .iter()
        .map(|e| e.event.message.as_ref().unwrap().len())
        .sum();
    assert!(total <= 9 * 1024 * 1024, "{total}");
    let rest = walk(&s, &EventQuery::default(), 1000);
    assert_eq!(rest.len(), 1000, "paging still reaches every event");
}

#[test]
fn review_r18_empty_range_does_not_report_unrelated_old_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "a"));
    s.flush(T).unwrap();
    s.shutdown(T).unwrap();
    drop(s);
    {
        let c = Connection::open(&path).unwrap();
        c.execute(
            "INSERT INTO runs (run_key, started_at, heartbeat_at, version, pid, last_seq)
             VALUES ('dead', 1, 1, 'test', 2147483000, 1)",
            [],
        )
        .unwrap();
    }
    let s = open(&path); // gap near seq 1..2
    for i in 0..10 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, "later"));
    }
    s.flush(T).unwrap();
    let newest = s.query(&EventQuery::default()).unwrap().newest_seq.unwrap();
    let p = s
        .query(&EventQuery {
            after_seq: Some(newest),
            ..Default::default()
        })
        .unwrap();
    assert!(p.events.is_empty());
    assert!(
        p.completeness.complete,
        "nothing new and no loss at the tail: {:?}",
        p.completeness
    );
}
