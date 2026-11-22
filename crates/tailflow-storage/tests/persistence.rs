//! Persistence, ordering, cursors, and query behavior.

mod common;

use chrono::{Duration as ChronoDuration, Utc};
use common::*;
use std::collections::HashSet;
use std::sync::Arc;
use tailflow_core::event::EventStatus;
use tailflow_core::LogLevel;
use tailflow_storage::{EventQuery, Order, Submitted};

#[test]
fn insert_query_and_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    for i in 0..50 {
        assert_eq!(
            s.submit(log_event(i, "api", LogLevel::Info, &format!("line {i}"))),
            Submitted::Enqueued
        );
    }
    s.flush(T).unwrap();
    assert_eq!(s.health().events_persisted, 50);
    let first = s.query(&EventQuery::default()).unwrap();
    assert_eq!(first.events.len(), 50);
    assert!(first.completeness.complete);
    s.shutdown(T).unwrap();
    drop(s);

    let s = open(&path);
    let page = s.query(&EventQuery::default()).unwrap();
    assert_eq!(page.events.len(), 50, "history must survive a restart");
    assert_eq!(page.events[0].event.message.as_deref(), Some("line 0"));
    assert!(page.completeness.complete, "a clean restart is not loss");
    assert!(s.recovered_runs().is_empty());
    assert_eq!(s.status().unwrap().db.events, 50);
}

#[test]
fn stable_ids_are_deduplicated_and_runtime_lines_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let e = agent_event("adapter-42", "agent.tool.started", "sess");
    let _ = s.submit(e.clone());
    let _ = s.submit(e.clone());
    s.flush(T).unwrap();
    let _ = s.submit(e); // replay after commit
                         // Identical runtime lines are distinct facts.
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "same"));
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "same"));
    s.flush(T).unwrap();
    let h = s.health();
    assert_eq!(h.duplicates_ignored, 2);
    assert_eq!(h.events_persisted, 3);
    assert_eq!(count_rows(&db_path(&dir)), 3);
}

#[test]
fn pagination_with_identical_timestamps_neither_skips_nor_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let when = Utc::now();
    for i in 0..503 {
        let _ = s.submit(at(log_event(i, "api", LogLevel::Info, "tick"), when));
    }
    s.flush(T).unwrap();
    let seqs = walk(&s, &EventQuery::default(), 7);
    assert_eq!(seqs.len(), 503);
    assert_eq!(
        seqs.iter().copied().collect::<HashSet<_>>().len(),
        503,
        "no duplicates"
    );
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "strictly increasing");

    // Descending pages too.
    let mut before = None;
    let mut desc = Vec::new();
    loop {
        let p = s
            .query(&EventQuery {
                order: Order::Desc,
                before_seq: before,
                limit: 11,
                ..Default::default()
            })
            .unwrap();
        desc.extend(p.events.iter().map(|e| e.seq));
        if !p.has_more {
            break;
        }
        before = p.next_cursor;
    }
    desc.reverse();
    assert_eq!(desc, seqs);
}

#[test]
fn cursor_reuse_across_restart_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    for i in 0..30 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, &format!("a{i}")));
    }
    s.flush(T).unwrap();
    let p = s
        .query(&EventQuery {
            limit: 30,
            ..Default::default()
        })
        .unwrap();
    let cursor = p.next_cursor.unwrap();
    s.shutdown(T).unwrap();
    drop(s);

    let s = open(&path);
    for i in 0..5 {
        let _ = s.submit(log_event(i, "api", LogLevel::Info, &format!("b{i}")));
    }
    s.flush(T).unwrap();
    let p = s
        .query(&EventQuery {
            after_seq: Some(cursor),
            ..Default::default()
        })
        .unwrap();
    let msgs: Vec<_> = p
        .events
        .iter()
        .map(|e| e.event.message.clone().unwrap())
        .collect();
    assert_eq!(msgs, vec!["b0", "b1", "b2", "b3", "b4"]);
    assert!(
        p.events.iter().all(|e| e.seq > cursor),
        "seq continues after restart"
    );
}

#[test]
fn concurrent_producers_get_unique_monotonic_seqs() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(open(&db_path(&dir)));
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let s = s.clone();
            std::thread::spawn(move || {
                for i in 0..500 {
                    let _ = s.submit(log_event(i, &format!("p{t}"), LogLevel::Info, "x"));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    s.flush(T).unwrap();
    assert_eq!(s.health().events_persisted, 2000);
    let seqs = walk(&s, &EventQuery::default(), 1000);
    assert_eq!(seqs.len(), 2000);
    // observed_at is non-decreasing in seq order: the invariant `since` relies on.
    let c = rusqlite::Connection::open(db_path(&dir)).unwrap();
    let bad: i64 = c
        .query_row(
            "SELECT count(*) FROM events a JOIN events b ON b.seq = a.seq + 1 WHERE b.observed_at < a.observed_at",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bad, 0);
}

#[test]
fn concurrent_readers_during_ingestion() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(open(&db_path(&dir)));
    let writer = {
        let s = s.clone();
        std::thread::spawn(move || {
            for i in 0..3000 {
                let _ = s.submit(log_event(i, "api", LogLevel::Error, "ERROR boom"));
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || {
                let mut last_newest = 0;
                for _ in 0..50 {
                    let p = s
                        .query(&EventQuery {
                            event_types: vec!["runtime.error".into()],
                            order: Order::Desc,
                            limit: 50,
                            ..Default::default()
                        })
                        .unwrap();
                    let newest = p.newest_seq.unwrap_or(0);
                    assert!(
                        newest >= last_newest,
                        "readers never see history move backwards"
                    );
                    last_newest = newest;
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    s.flush(T).unwrap();
    assert_eq!(walk(&s, &EventQuery::default(), 1000).len(), 3000);
}

#[test]
fn late_and_future_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "now"));
    // Arrives now, happened an hour ago.
    let mut late = log_event(2, "api", LogLevel::Info, "late");
    late.timestamp = Utc::now() - ChronoDuration::hours(1);
    let _ = s.submit(late);
    // Claims to be from tomorrow.
    let mut future = log_event(3, "api", LogLevel::Info, "future");
    future.timestamp = Utc::now() + ChronoDuration::days(1);
    let _ = s.submit(future);
    s.flush(T).unwrap();

    let p = s.query(&EventQuery::default()).unwrap();
    let msgs: Vec<_> = p
        .events
        .iter()
        .map(|e| e.event.message.clone().unwrap())
        .collect();
    assert_eq!(
        msgs,
        vec!["now", "late", "future"],
        "ordered by arrival, not timestamp"
    );
    let fut = &p.events[2].event;
    assert!(fut.timestamp <= fut.observed_at, "future timestamp clamped");
    assert!(
        fut.attributes.contains_key("source_timestamp"),
        "original kept"
    );

    // `since` sees the late event (by its own timestamp) and the bound is exact.
    let p = s
        .query(&EventQuery {
            since: Some(Utc::now() - ChronoDuration::hours(2)),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(p.events.len(), 3);
    let p = s
        .query(&EventQuery {
            since: Some(Utc::now() - ChronoDuration::minutes(30)),
            ..Default::default()
        })
        .unwrap();
    let msgs: Vec<_> = p
        .events
        .iter()
        .map(|e| e.event.message.clone().unwrap())
        .collect();
    assert_eq!(msgs, vec!["now", "future"]);
}

#[test]
fn filters_combine_and_use_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let _ = s.submit(log_event(
        1,
        "api",
        LogLevel::Error,
        "ERROR connection refused: postgres:5432",
    ));
    let _ = s.submit(log_event(2, "web", LogLevel::Warn, "slow request"));
    let _ = s.submit(log_event(3, "api", LogLevel::Info, "GET /health 200"));
    let mut cmd = agent_event("c1", "command.failed", "abc123");
    cmd.status = Some(EventStatus::Failed);
    cmd.attributes
        .insert("command".into(), "cargo test --all".into());
    let _ = s.submit(cmd);
    let mut file = agent_event("f1", "file.modified", "abc123");
    file.path = Some("/work/app/src/auth/login.rs".into());
    let _ = s.submit(file);
    let mut other = agent_event("f2", "file.modified", "zzz");
    other.path = Some("/work/app/src/auth.rs".into());
    let _ = s.submit(other);
    s.flush(T).unwrap();

    let q = |q: EventQuery| s.query(&q).unwrap().events.len();
    assert_eq!(
        q(EventQuery {
            event_types: vec!["runtime.error".into()],
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        q(EventQuery {
            event_types: vec!["runtime.*".into()],
            ..Default::default()
        }),
        3
    );
    assert_eq!(
        q(EventQuery {
            event_types: vec!["file.*".into()],
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        q(EventQuery {
            min_severity: Some(LogLevel::Warn),
            sources: vec!["api".into()],
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        q(EventQuery {
            sessions: vec!["abc123".into()],
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        q(EventQuery {
            agents: vec!["claude".into()],
            event_types: vec!["file.modified".into()],
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        q(EventQuery {
            statuses: vec![EventStatus::Failed],
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        q(EventQuery {
            command: Some("cargo test".into()),
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        q(EventQuery {
            path_glob: Some("**/auth/**".into()),
            ..Default::default()
        }),
        1,
        "`**/auth/**` needs a directory named auth"
    );
    assert_eq!(
        q(EventQuery {
            path_prefix: Some("/work/app/src/auth".into()),
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        q(EventQuery {
            projects: vec!["/work/app".into()],
            ..Default::default()
        }),
        6,
        "project defaults from the handle"
    );
    assert_eq!(
        q(EventQuery {
            text: Some("connection refused".into()),
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        q(EventQuery {
            text: Some("postg*".into()),
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        q(EventQuery {
            text: Some("NEAR(".into()),
            ..Default::default()
        }),
        0,
        "FTS syntax is inert"
    );
    assert_eq!(
        q(EventQuery {
            text: Some("refused".into()),
            sources: vec!["web".into()],
            ..Default::default()
        }),
        0
    );
}

#[test]
fn empty_results_are_reliable_and_carry_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let p = s.query(&EventQuery::default()).unwrap();
    assert!(p.events.is_empty() && !p.has_more && p.completeness.complete);
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "x"));
    s.flush(T).unwrap();
    let p = s
        .query(&EventQuery {
            event_types: vec!["git.commit".into()],
            ..Default::default()
        })
        .unwrap();
    assert!(p.events.is_empty());
    assert!(!p.has_more);
    assert_eq!(
        p.next_cursor, p.newest_seq,
        "a tailing client resumes at the edge"
    );
}

#[test]
fn scan_budget_returns_an_advancing_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    s.shutdown(T).unwrap();
    drop(s);
    // Bulk-load 1.2M seq space cheaply by bumping the sequence, then one match
    // at the far end — an unindexed severity filter must page, not scan it all.
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("UPDATE sqlite_sequence SET seq = 1200000 WHERE name = 'events'")
            .unwrap();
    }
    let s = open(&path);
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "early"));
    s.flush(T).unwrap();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("UPDATE sqlite_sequence SET seq = 3000000 WHERE name = 'events'")
            .unwrap();
    }
    let _ = s.submit(log_event(2, "api", LogLevel::Error, "late error"));
    s.flush(T).unwrap();
    let base = EventQuery {
        min_severity: Some(LogLevel::Error),
        ..Default::default()
    };
    let first = s.query(&base).unwrap();
    assert!(first.events.is_empty());
    assert!(first.scan_budget_exhausted && first.has_more);
    let mut after = first.next_cursor;
    let mut found = Vec::new();
    for _ in 0..10 {
        let p = s
            .query(&EventQuery {
                after_seq: after,
                ..base.clone()
            })
            .unwrap();
        assert!(p.next_cursor > after, "cursor must advance");
        found.extend(p.events);
        if !p.has_more {
            break;
        }
        after = p.next_cursor;
    }
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].event.message.as_deref(), Some("late error"));
}

#[test]
fn invalid_queries_fail_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let err = s
        .query(&EventQuery {
            event_types: vec!["file.modifed".into()],
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.kind(), "invalid_query");
    assert!(err.public_message().contains("file.modifed"));
}

#[test]
fn get_by_seq_and_expiry_are_distinguished() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "one"));
    s.flush(T).unwrap();
    let seq = s.query(&EventQuery::default()).unwrap().events[0].seq;
    assert!(s.get(seq, false).unwrap().is_some());
    assert!(s.get(seq + 100, false).unwrap().is_none());
    assert!(!s.is_expired(seq).unwrap());
    tailflow_storage::admin::purge(&path, tailflow_storage::admin::PurgeTarget::All).unwrap();
    assert!(s.get(seq, false).unwrap().is_none());
    assert!(
        s.is_expired(seq).unwrap(),
        "purged history is expired, not unknown"
    );
}
