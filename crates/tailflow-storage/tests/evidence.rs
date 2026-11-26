//! File-change evidence statuses (M4-C) and the observation-coverage claim.

mod common;

use chrono::{DateTime, Duration as CD, Utc};
use common::*;
use std::time::Duration;
use tailflow_core::event::{ObservabilityEvent, Provenance};
use tailflow_storage::evidence::FileEvidenceQuery;
use tailflow_storage::{CoverageClaim, LossReason};

const ROOT: &str = "/work/app";

fn reported(
    id: &str,
    ty: &str,
    session: &str,
    path: &str,
    at: DateTime<Utc>,
) -> ObservabilityEvent {
    let mut e = agent_event(id, ty, session);
    e.source_type = "agent_hook".into();
    e.path = Some(path.into());
    e.timestamp = at;
    e
}

fn observed(id: &str, path: &str, at: DateTime<Utc>) -> ObservabilityEvent {
    let mut e = ObservabilityEvent::new(
        id,
        "file.modified",
        Provenance::FilesystemObserved,
        "fs_watcher",
        "fs:/work/app",
        at,
    );
    e.path = Some(path.into());
    e
}

fn query(since: DateTime<Utc>, until: DateTime<Utc>) -> FileEvidenceQuery {
    FileEvidenceQuery {
        since,
        until,
        session: None,
        agent: None,
        path_prefix: None,
        window: Duration::from_secs(5),
        limit: 100,
    }
}

fn status_of<'a>(
    page: &'a tailflow_storage::evidence::FileEvidencePage,
    path: &str,
) -> Vec<&'a str> {
    page.items
        .iter()
        .filter(|i| i.path == path)
        .map(|i| i.status)
        .collect()
}

#[test]
fn reported_and_observed_changes_get_documented_statuses() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let claim = s.coverage_started(ROOT, "test", 10, 0).unwrap();
    assert!(matches!(claim, CoverageClaim::Claimed(_)));
    // Storage clamps future timestamps to the time it observed the event,
    // so events are stamped "now" (after coverage started); offsets within
    // the window are in the past.
    std::thread::sleep(Duration::from_millis(5));
    let t = Utc::now();
    std::thread::sleep(Duration::from_millis(5));
    let p = |n: &str| format!("{ROOT}/{n}");
    let events = [
        // matched: reported + observed 1 s later.
        reported("r1", "file.modified", "s1", &p("a.rs"), t),
        observed("o1", &p("a.rs"), t + CD::seconds(1)),
        // reported only: covered, nothing observed.
        reported("r2", "file.modified", "s1", &p("b.rs"), t),
        // unverified: outside every observed root.
        reported("r3", "file.created", "s1", "/elsewhere/c.rs", t),
        // filesystem observed: nobody reported it.
        observed("o4", &p("human.rs"), t),
        // ambiguous: two sessions reported the one observed change.
        reported("r5a", "file.modified", "s1", &p("shared.rs"), t),
        reported(
            "r5b",
            "file.modified",
            "s2",
            &p("shared.rs"),
            t + CD::seconds(1),
        ),
        observed("o5", &p("shared.rs"), t + CD::seconds(1)),
        // An observation far outside the window does not match a report.
        reported("r6", "file.modified", "s1", &p("late.rs"), t),
        observed("o6", &p("late.rs"), t - CD::seconds(40)),
    ];
    for e in events {
        let _ = s.submit(e);
    }
    s.flush(T).unwrap();
    let page = s
        .file_evidence(&query(t - CD::seconds(60), t + CD::seconds(60)))
        .unwrap();
    assert_eq!(status_of(&page, &p("a.rs")), ["matched_observation"]);
    assert_eq!(status_of(&page, &p("b.rs")), ["reported_only"]);
    assert_eq!(status_of(&page, "/elsewhere/c.rs"), ["unverified"]);
    assert_eq!(status_of(&page, &p("human.rs")), ["filesystem_observed"]);
    let shared = status_of(&page, &p("shared.rs"));
    assert!(
        !shared.is_empty() && shared.iter().all(|s| *s == "ambiguous"),
        "{shared:?}"
    );
    let late = status_of(&page, &p("late.rs"));
    assert!(
        late.contains(&"reported_only") && late.contains(&"filesystem_observed"),
        "{late:?}"
    );
    // Every item explains itself and references real events.
    for i in &page.items {
        assert!(!i.explanation.is_empty());
        assert!(!i.reported.is_empty() || !i.observed.is_empty());
    }
    assert!(page
        .items
        .iter()
        .filter(|i| i.status == "matched_observation")
        .all(|i| i.explanation.contains("not proof")));
    assert_eq!(page.coverage.len(), 1);

    // Session scope: only that session's reports.
    let mut q = query(t - CD::seconds(60), t + CD::seconds(60));
    q.session = Some("s2".into());
    let scoped = s.file_evidence(&q).unwrap();
    assert!(scoped.items.iter().all(|i| i
        .reported
        .iter()
        .any(|r| r.session_id.as_deref() == Some("s2"))));
}

#[test]
fn observer_loss_and_partial_coverage_make_reports_unverifiable() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let _ = s.coverage_started(ROOT, "test", 10, 3).unwrap(); // partial
    std::thread::sleep(Duration::from_millis(5));
    let t = Utc::now();
    let _ = s.submit(reported(
        "r1",
        "file.modified",
        "s1",
        &format!("{ROOT}/a.rs"),
        t,
    ));
    s.flush(T).unwrap();
    let page = s
        .file_evidence(&query(t - CD::seconds(60), t + CD::seconds(60)))
        .unwrap();
    assert_eq!(page.items[0].status, "unverified");
    assert!(
        page.items[0].explanation.contains("partial"),
        "{}",
        page.items[0].explanation
    );

    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let _ = s.coverage_started(ROOT, "test", 10, 0).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let t = Utc::now();
    s.record_observer_loss(
        LossReason::FsWatcherRescan,
        1,
        t - CD::seconds(1),
        t + CD::seconds(1),
    );
    let _ = s.submit(reported(
        "r1",
        "file.modified",
        "s1",
        &format!("{ROOT}/a.rs"),
        t,
    ));
    s.flush(T).unwrap();
    let page = s
        .file_evidence(&query(t - CD::seconds(60), t + CD::seconds(60)))
        .unwrap();
    assert_eq!(page.items[0].status, "unverified");
    assert!(page.items[0].explanation.contains("loss"));
    assert_eq!(page.loss.len(), 1);
    assert_eq!(s.health().events_dropped.filesystem_observer, 1);
}

#[test]
fn reports_before_coverage_started_are_unverified() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let t = Utc::now() - CD::minutes(10);
    let _ = s.submit(reported(
        "r1",
        "file.modified",
        "s1",
        &format!("{ROOT}/a.rs"),
        t,
    ));
    s.flush(T).unwrap();
    let _ = s.coverage_started(ROOT, "test", 10, 0).unwrap();
    let page = s
        .file_evidence(&query(t - CD::seconds(60), t + CD::seconds(60)))
        .unwrap();
    assert_eq!(page.items[0].status, "unverified");
}

#[test]
fn one_root_is_observed_by_one_live_daemon_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let a = open(&path);
    let b = open(&path);
    let id = match a.coverage_started(ROOT, "test", 1, 0).unwrap() {
        CoverageClaim::Claimed(id) => id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        b.coverage_started(ROOT, "test", 1, 0).unwrap(),
        CoverageClaim::HeldBy { .. }
    ));
    assert!(matches!(
        b.coverage_started("/other/root", "test", 1, 0).unwrap(),
        CoverageClaim::Claimed(_)
    ));
    a.coverage_stopped(id).unwrap();
    assert!(matches!(
        b.coverage_started(ROOT, "test", 1, 0).unwrap(),
        CoverageClaim::Claimed(_)
    ));
}

#[test]
fn evidence_queries_are_validated_and_like_safe() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let now = Utc::now();
    let mut q = query(now, now - CD::seconds(1));
    assert!(s.file_evidence(&q).is_err(), "inverted range");
    q = query(now - CD::hours(1), now);
    q.window = Duration::from_secs(61);
    assert!(s.file_evidence(&q).is_err());
    q.window = Duration::from_secs(5);
    q.limit = 0;
    assert!(s.file_evidence(&q).is_err());
    q.limit = 10;
    // `%` and `_` in a prefix are literal, not wildcards.
    let _ = s.coverage_started(ROOT, "test", 1, 0).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let t = Utc::now();
    let _ = s.submit(observed("o1", &format!("{ROOT}/x_y.rs"), t));
    let _ = s.submit(observed("o2", &format!("{ROOT}/xzy.rs"), t));
    s.flush(T).unwrap();
    let mut q = query(t - CD::seconds(60), t + CD::seconds(60));
    q.path_prefix = Some(format!("{ROOT}/x_"));
    let page = s.file_evidence(&q).unwrap();
    assert_eq!(page.items.len(), 1, "{:?}", page.items);
    assert!(page.items[0].path.ends_with("x_y.rs"));
}
