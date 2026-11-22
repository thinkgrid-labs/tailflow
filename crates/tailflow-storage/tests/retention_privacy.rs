//! Retention, privacy-before-disk, and deletion visibility.

mod common;

use common::*;
use std::sync::Arc;
use tailflow_core::config::{Retention, StorageConfig};
use tailflow_core::privacy::{CaptureMode, PrivacyConfig};
use tailflow_core::LogLevel;
use tailflow_storage::{admin, EventQuery, Storage, Submitted};

fn open_cfg(cfg: &StorageConfig, privacy: &PrivacyConfig) -> Storage {
    Storage::open(cfg, opts(privacy)).unwrap()
}

#[test]
fn age_expiry_boundary_is_deterministic_and_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    for (i, days) in [40, 31, 29, 1, 0].iter().enumerate() {
        let _ = s.submit(at(
            log_event(i as u64, "api", LogLevel::Info, &format!("{days}d")),
            ago(*days),
        ));
    }
    s.flush(T).unwrap();
    // The startup pass may already have run; either way the outcome is fixed.
    s.run_retention(T).unwrap();
    let p = s.query(&EventQuery::default()).unwrap();
    let msgs: Vec<_> = p
        .events
        .iter()
        .map(|e| e.event.message.clone().unwrap())
        .collect();
    assert_eq!(msgs, vec!["29d", "1d", "0d"]);
    assert!(p.completeness.complete, "expiry is not loss");
    assert!(
        p.completeness.retention_truncated,
        "but the range is truncated"
    );
    assert_eq!(p.completeness.evicted_through_seq, 2);
    assert_eq!(s.status().unwrap().db.events, 3, "counter tracks deletes");
}

#[test]
fn retention_policy_change_applies_on_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    for (i, days) in [20, 10, 2].iter().enumerate() {
        let _ = s.submit(at(
            log_event(i as u64, "api", LogLevel::Info, "x"),
            ago(*days),
        ));
    }
    s.flush(T).unwrap();
    s.shutdown(T).unwrap();
    drop(s);
    let mut cfg = config(&path);
    cfg.retention_days = Retention::Days(7);
    let s = open_cfg(&cfg, &PrivacyConfig::default());
    s.run_retention(T).unwrap();
    assert_eq!(s.query(&EventQuery::default()).unwrap().events.len(), 1);
    s.shutdown(T).unwrap();
    drop(s);
    // Raising retention cannot resurrect anything; the horizon says so.
    cfg.retention_days = Retention::Unlimited;
    let s = open_cfg(&cfg, &PrivacyConfig::default());
    let p = s.query(&EventQuery::default()).unwrap();
    assert_eq!(p.events.len(), 1);
    assert_eq!(p.completeness.evicted_through_seq, 2);
}

#[test]
fn size_cap_evicts_oldest_first_and_reclaims_space() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut cfg = config(&path);
    cfg.max_size_mb = 16;
    cfg.retention_days = Retention::Unlimited;
    cfg.full_text_search = false;
    let s = open_cfg(&cfg, &PrivacyConfig::default());
    let body = "x".repeat(8 * 1024);
    for i in 0..4_000u64 {
        let _ = s.submit(log_event(
            i,
            "api",
            LogLevel::Info,
            &format!("{i:06} {body}"),
        ));
    }
    s.flush(T).unwrap();
    let before = s.status().unwrap().db;
    assert!(
        before.used_bytes > 16 * 1024 * 1024,
        "test must exceed the cap: {}",
        before.used_bytes
    );
    let mut evicted = 0;
    for _ in 0..50 {
        let r = s.run_retention(T).unwrap();
        evicted += r.evicted_by_size;
        if !r.more_work {
            break;
        }
    }
    let after = s.status().unwrap().db;
    assert!(evicted > 0);
    assert!(
        after.used_bytes <= 16 * 1024 * 1024,
        "used {} > cap",
        after.used_bytes
    );
    assert!(
        after.file_bytes < before.file_bytes,
        "incremental vacuum returned space"
    );
    let p = s
        .query(&EventQuery {
            limit: 1,
            ..Default::default()
        })
        .unwrap();
    let oldest_kept: u64 = p.events[0].event.message.as_deref().unwrap()[..6]
        .parse()
        .unwrap();
    assert_eq!(
        oldest_kept, evicted,
        "exactly the oldest {evicted} were evicted"
    );
    assert_eq!(after.events, 4_000 - evicted);
}

#[test]
fn cleanup_runs_safely_during_ingestion() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = Arc::new(open(&path));
    for i in 0..2_000u64 {
        let _ = s.submit(at(log_event(i, "api", LogLevel::Info, "old"), ago(45)));
    }
    s.flush(T).unwrap();
    let producer = {
        let s = s.clone();
        std::thread::spawn(move || {
            for i in 0..5_000u64 {
                let _ = s.submit(log_event(i, "api", LogLevel::Info, "new"));
            }
        })
    };
    for _ in 0..20 {
        s.run_retention(T).unwrap();
    }
    producer.join().unwrap();
    s.flush(T).unwrap();
    s.run_retention(T).unwrap();
    assert_eq!(
        count_rows(&path),
        5_000,
        "all 2,000 expired rows gone, no new row lost"
    );
    assert_eq!(
        s.status().unwrap().db.events,
        5_000,
        "meta counter == COUNT(*)"
    );
    assert_eq!(s.health().events_dropped_total, 0);
}

#[test]
fn payloads_expire_on_their_own_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let privacy = PrivacyConfig {
        capture: CaptureMode::Redacted,
        ..Default::default()
    };
    let s = open_cfg(&config(&path), &privacy);
    for (i, days) in [10, 1].iter().enumerate() {
        let mut e = at(
            agent_event(&format!("t{i}"), "agent.tool.completed", "s"),
            ago(*days),
        );
        e.payload = Some(format!("tool output {i}"));
        let _ = s.submit(e);
    }
    s.flush(T).unwrap();
    // The startup pass may already have cleared it; the end state is fixed.
    s.run_retention(T).unwrap();
    let p = s
        .query(&EventQuery {
            include_payload: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(p.events.len(), 2, "events stay; only the old body goes");
    assert!(p.events[0].event.payload.is_none());
    assert_eq!(p.events[1].event.payload.as_deref(), Some("tool output 1"));
}

#[test]
fn secrets_never_reach_the_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let privacy = PrivacyConfig {
        capture: CaptureMode::Redacted,
        ..Default::default()
    };
    let s = open_cfg(&config(&path), &privacy);
    let secret = "hunter2-zq7Xv9Lm";
    let _ = s.submit(log_event(
        1,
        "api",
        LogLevel::Info,
        &format!("login password={secret}"),
    ));
    let mut e = agent_event("a", "command.failed", "s");
    e.attributes
        .insert("env".into(), format!("DB_PASSWORD={secret}").into());
    e.payload = Some(format!("Authorization: Bearer {secret}abcdef"));
    let _ = s.submit(e);
    s.flush(T).unwrap();
    s.shutdown(T).unwrap();
    let bytes = disk_bytes(&path);
    assert!(
        !contains(&bytes, secret),
        "secret found in the database or WAL"
    );
    assert!(contains(&bytes, "[REDACTED:secret_assignment]"));
}

#[test]
fn metadata_mode_keeps_no_payload_bodies_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    let mut e = agent_event("a", "agent.tool.completed", "s");
    e.payload = Some("SOURCE-FILE-CONTENTS-unique-marker".into());
    let _ = s.submit(e);
    s.flush(T).unwrap();
    let p = s
        .query(&EventQuery {
            include_payload: true,
            ..Default::default()
        })
        .unwrap();
    assert!(p.events[0].event.payload.is_none());
    assert!(p.events[0].event.redaction.payload_dropped);
    s.shutdown(T).unwrap();
    assert!(!contains(
        &disk_bytes(&path),
        "SOURCE-FILE-CONTENTS-unique-marker"
    ));
}

#[test]
fn collection_boundaries_exclude_before_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let privacy = PrivacyConfig {
        excluded_file_globs: vec!["**/.env".into()],
        excluded_event_types: vec!["runtime.log".into()],
        ..Default::default()
    };
    let s = open_cfg(&config(&path), &privacy);
    let mut env = agent_event("a", "file.read", "s");
    env.path = Some("/work/app/.env-marker/.env".into());
    assert_eq!(s.submit(env), Submitted::Excluded);
    assert_eq!(
        s.submit(log_event(1, "api", LogLevel::Info, "plain-line-marker")),
        Submitted::Excluded
    );
    assert_eq!(
        s.submit(log_event(2, "api", LogLevel::Error, "ERROR kept")),
        Submitted::Enqueued
    );
    s.flush(T).unwrap();
    let h = s.health();
    assert_eq!(h.events_excluded, 2);
    assert!(!h.loss_detected, "exclusion is deliberate, not loss");
    s.shutdown(T).unwrap();
    let bytes = disk_bytes(&path);
    assert!(!contains(&bytes, "plain-line-marker"));
    assert!(!contains(&bytes, ".env-marker"));
}

#[test]
fn capture_disabled_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let privacy = PrivacyConfig {
        capture_enabled: false,
        ..Default::default()
    };
    let s = open_cfg(&config(&path), &privacy);
    assert_eq!(
        s.submit(log_event(1, "api", LogLevel::Error, "x")),
        Submitted::Excluded
    );
    s.flush(T).unwrap();
    assert_eq!(count_rows(&path), 0);
}

#[test]
fn deleted_session_is_visible_in_completeness() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    let _ = s.submit(log_event(1, "api", LogLevel::Info, "before"));
    for i in 0..10 {
        let _ = s.submit(agent_event(
            &format!("x{i}"),
            "agent.tool.started",
            "doomed",
        ));
    }
    let _ = s.submit(log_event(2, "api", LogLevel::Info, "after"));
    s.flush(T).unwrap();
    let r = admin::purge(&path, admin::PurgeTarget::Session("doomed")).unwrap();
    assert_eq!(r.deleted, 10);
    let p = s.query(&EventQuery::default()).unwrap();
    assert_eq!(p.events.len(), 2);
    assert!(!p.completeness.complete);
    assert!(p.completeness.reasons.contains(&"deleted_in_range"));
    assert_eq!(p.completeness.gaps[0].count, Some(10));
    assert!(s
        .query(&EventQuery {
            sessions: vec!["doomed".into()],
            ..Default::default()
        })
        .unwrap()
        .events
        .is_empty());
    assert_eq!(s.status().unwrap().db.events, 2);
}

#[test]
fn purge_before_and_all() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let s = open(&path);
    let _ = s.submit(at(log_event(1, "api", LogLevel::Info, "old"), ago(3)));
    let _ = s.submit(log_event(2, "api", LogLevel::Info, "new"));
    s.flush(T).unwrap();
    let r = admin::purge(&path, admin::PurgeTarget::Before(ago(1))).unwrap();
    assert_eq!(r.deleted, 1);
    let r = admin::purge(&path, admin::PurgeTarget::All).unwrap();
    assert_eq!(r.deleted, 1);
    assert_eq!(count_rows(&path), 0);
    assert!(admin::check(&path).unwrap().is_empty());
    // The store keeps working after a purge.
    let _ = s.submit(log_event(3, "api", LogLevel::Info, "again"));
    s.flush(T).unwrap();
    assert_eq!(count_rows(&path), 1);
}
