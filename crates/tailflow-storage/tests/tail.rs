//! The committed-history tail behind the live agent stream (M4-B):
//! commit notifications, `newest_seq`, and the guarantee that a reader
//! following `seq > cursor` sees every committed event exactly once, even
//! with several writers (daemons) sharing one database file.

mod common;

use common::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tailflow_core::LogLevel;
use tailflow_storage::{EventQuery, Order};

#[test]
fn commit_hook_fires_after_commits_that_insert() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let fired = Arc::new(AtomicUsize::new(0));
    let non_runtime = Arc::new(AtomicUsize::new(0));
    let (f, nr) = (fired.clone(), non_runtime.clone());
    s.on_commit(move |info| {
        f.fetch_add(1, Ordering::SeqCst);
        nr.fetch_add(
            usize::try_from(info.inserted_non_runtime).unwrap(),
            Ordering::SeqCst,
        );
    });
    assert_eq!(s.newest_seq().unwrap(), 0);
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "one"));
    s.flush(T).unwrap();
    assert!(fired.load(Ordering::SeqCst) >= 1);
    assert_eq!(s.newest_seq().unwrap(), 1);
    assert_eq!(
        non_runtime.load(Ordering::SeqCst),
        0,
        "a log line is runtime"
    );
    // A duplicate inserts nothing: no notification.
    let e = agent_event("stable-1", "agent.tool.started", "sess");
    let _ = s.submit(e.clone());
    s.flush(T).unwrap();
    let after_first = fired.load(Ordering::SeqCst);
    assert_eq!(
        non_runtime.load(Ordering::SeqCst),
        1,
        "an agent event is not"
    );
    let _ = s.submit(e);
    s.flush(T).unwrap();
    assert_eq!(fired.load(Ordering::SeqCst), after_first);
}

#[test]
fn a_seq_tail_sees_every_commit_from_concurrent_writers_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    // Two handles on one file behave like two daemons: separate writer
    // threads and connections, serialized only by SQLite's write lock.
    let a = open(&path);
    let b = open(&path);
    let reader = open(&path);
    const PER_WRITER: usize = 600;
    let writers: Vec<_> = [(a, "a"), (b, "b")]
        .into_iter()
        .map(|(s, tag)| {
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    let _ = s.submit(agent_event(
                        &format!("{tag}-{i}"),
                        "agent.tool.started",
                        &format!("sess-{tag}"),
                    ));
                    if i % 50 == 0 {
                        s.flush(T).unwrap();
                    }
                }
                s.flush(T).unwrap();
                s
            })
        })
        .collect();

    // Tail while they write.
    let mut cursor = 0u64;
    let mut seen = HashSet::new();
    let mut last_seq = 0u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while seen.len() < 2 * PER_WRITER {
        assert!(
            std::time::Instant::now() < deadline,
            "tail stalled at {}",
            seen.len()
        );
        let page = reader
            .query(&EventQuery {
                event_types: vec!["agent.*".into()],
                after_seq: Some(cursor),
                order: Order::Asc,
                limit: 200,
                ..EventQuery::default()
            })
            .unwrap();
        for e in &page.events {
            assert!(e.seq > last_seq, "seq order is strictly increasing");
            last_seq = e.seq;
            assert!(
                seen.insert(e.event.event_id.clone()),
                "duplicate {}",
                e.event.event_id
            );
        }
        if let Some(c) = page.next_cursor {
            cursor = c;
        }
        assert!(reader.newest_seq().unwrap() >= cursor);
        if page.events.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    for w in writers {
        let s = w.join().unwrap();
        s.shutdown(T).unwrap();
    }
    assert_eq!(seen.len(), 2 * PER_WRITER, "every committed event, once");
}
