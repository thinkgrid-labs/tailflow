//! Dashboard aggregates (`stats`, `files`) and session-list pagination.

mod common;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::*;
use std::collections::HashSet;
use tailflow_core::event::{EventStatus, ObservabilityEvent, Provenance};
use tailflow_core::LogLevel;
use tailflow_storage::sessions::SessionQuery;
use tailflow_storage::stats::{FilesQuery, StatsQuery};
use tailflow_storage::{Storage, Submitted};

fn ev(
    id: &str,
    ty: &str,
    agent: &str,
    session: &str,
    project: &str,
    when: DateTime<Utc>,
) -> ObservabilityEvent {
    let mut e = ObservabilityEvent::new(
        id,
        ty,
        Provenance::AgentReported,
        tailflow_core::agent::SOURCE_TYPE,
        agent,
        when,
    );
    e.observed_at = when;
    e.session_id = Some(format!("{agent}:{session}"));
    e.agent_name = Some(agent.into());
    e.project_id = Some(project.into());
    e.stable_id = true;
    if ty.ends_with(".failed") {
        e.status = Some(EventStatus::Failed);
        e.severity = LogLevel::Error;
    }
    e
}

fn file_ev(id: &str, ty: &str, agent: &str, session: &str, path: &str) -> ObservabilityEvent {
    let mut e = ev(id, ty, agent, session, "/work/app", Utc::now());
    e.path = Some(path.into());
    e
}

fn submit_all(s: &Storage, events: Vec<ObservabilityEvent>) {
    for e in events {
        assert_eq!(s.submit(e), Submitted::Enqueued);
    }
    s.flush(T).unwrap();
}

fn hour() -> StatsQuery {
    StatsQuery {
        since: Utc::now() - ChronoDuration::hours(1),
        until: None,
        projects: Vec::new(),
        agents: Vec::new(),
        buckets: 12,
    }
}

/// Two agents, two projects, runtime noise: every overview number is exact.
fn seed(s: &Storage) {
    let now = Utc::now() - ChronoDuration::minutes(5);
    let t = |m: i64| now + ChronoDuration::seconds(m);
    let mut events = vec![
        ev(
            "c1",
            "agent.session.started",
            "claude-code",
            "a",
            "/work/app",
            t(0),
        ),
        ev(
            "c2",
            "agent.tool.completed",
            "claude-code",
            "a",
            "/work/app",
            t(1),
        ),
        ev(
            "c3",
            "agent.tool.failed",
            "claude-code",
            "a",
            "/work/app",
            t(2),
        ),
        ev(
            "c4",
            "command.started",
            "claude-code",
            "a",
            "/work/app",
            t(2),
        ),
        ev(
            "c5",
            "command.failed",
            "claude-code",
            "a",
            "/work/app",
            t(3),
        ),
        ev(
            "c6",
            "agent.session.ended",
            "claude-code",
            "a",
            "/work/app",
            t(4),
        ),
        ev(
            "x1",
            "agent.session.started",
            "codex",
            "b",
            "/work/web",
            t(1),
        ),
        ev(
            "x2",
            "agent.tool.completed",
            "codex",
            "b",
            "/work/web",
            t(2),
        ),
        ev("x3", "command.started", "codex", "b", "/work/web", t(3)),
    ];
    let mut f1 = ev("c7", "file.read", "claude-code", "a", "/work/app", t(1));
    f1.path = Some("/work/app/src/lib.rs".into());
    let mut f2 = ev("x4", "file.modified", "codex", "b", "/work/web", t(3));
    f2.path = Some("/work/web/index.ts".into());
    events.push(f1);
    events.push(f2);
    for i in 0..30u64 {
        let level = match i % 3 {
            0 => LogLevel::Error,
            1 => LogLevel::Warn,
            _ => LogLevel::Info,
        };
        events.push(log_event(i, "api", level, "line"));
    }
    submit_all(s, events);
}

#[test]
fn overview_counts_are_exact_per_agent_and_overall() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    seed(&s);
    let st = s.stats(&hour()).unwrap();

    assert_eq!(st.events_ingested, 41, "every ingested event, all sources");
    assert_eq!(st.runtime_errors, 10);
    assert_eq!(st.runtime_warnings, 10);
    assert_eq!(st.activity.tool_calls, 3, "completed + failed");
    assert_eq!(st.activity.tool_failures, 1);
    assert_eq!(st.activity.commands, 2);
    assert_eq!(st.activity.command_failures, 1);
    // c3 (tool failure) and c5 (command failure) are separate events with
    // no shared operation id, so both count.
    assert_eq!(st.activity.failures, 2);
    assert_eq!(st.activity.file_events, 2);
    assert_eq!(st.activity.file_reads, 1);
    assert_eq!(st.activity.file_changes, 1);

    assert_eq!(st.sessions.total, 2);
    assert_eq!(st.sessions.ended, 1);
    assert_eq!(st.sessions.active, 1);
    assert_eq!(st.sessions.unterminated, 0);

    let claude = st.agents.iter().find(|a| a.agent == "claude-code").unwrap();
    assert_eq!(claude.sessions, 1);
    assert_eq!(claude.active_sessions, 0);
    assert_eq!(claude.activity.tool_calls, 2);
    assert_eq!(claude.latest_session_status, Some("ended"));
    let codex = st.agents.iter().find(|a| a.agent == "codex").unwrap();
    assert_eq!(codex.active_sessions, 1);
    assert_eq!(codex.activity.file_changes, 1);
    assert_eq!(codex.latest_session_id.as_deref(), Some("codex:b"));

    // Buckets partition the same numbers.
    assert_eq!(st.buckets.len(), 12);
    let sum =
        |f: fn(&tailflow_storage::stats::Bucket) -> u64| st.buckets.iter().map(f).sum::<u64>();
    assert_eq!(sum(|b| b.ingested), st.events_ingested);
    assert_eq!(sum(|b| b.runtime_errors), st.runtime_errors);
    assert_eq!(sum(|b| b.failures), st.activity.failures);
    assert_eq!(sum(|b| b.agent_events), 11);
    assert!(!st.retention_truncated);
    assert_eq!(st.gaps_in_range, 0);
}

#[test]
fn overview_filters_by_agent_and_project() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    seed(&s);
    let codex = s
        .stats(&StatsQuery {
            agents: vec!["openai-codex".into()],
            ..hour()
        })
        .unwrap();
    assert_eq!(codex.activity.tool_calls, 1, "alias resolves to codex");
    assert_eq!(codex.sessions.total, 1);
    assert_eq!(codex.agents.len(), 1);

    let app = s
        .stats(&StatsQuery {
            projects: vec!["/work/app".into()],
            ..hour()
        })
        .unwrap();
    assert_eq!(app.activity.tool_calls, 2);
    assert_eq!(app.sessions.total, 1);
}

#[test]
fn overview_range_excludes_older_activity_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let old = ago(2);
    submit_all(
        &s,
        vec![
            ev(
                "o1",
                "agent.tool.completed",
                "claude-code",
                "old",
                "/work/app",
                old,
            ),
            at(log_event(1, "api", LogLevel::Error, "old"), old),
        ],
    );
    let st = s.stats(&hour()).unwrap();
    assert_eq!(st.activity.tool_calls, 0);
    assert_eq!(st.runtime_errors, 0);
    assert_eq!(st.events_ingested, 0);
    assert_eq!(st.sessions.total, 0);
    // The agent still has a latest session, outside the range.
    let claude = st.agents.iter().find(|a| a.agent == "claude-code").unwrap();
    assert_eq!(claude.latest_session_id.as_deref(), Some("claude-code:old"));
    assert_eq!(claude.latest_session_status, Some("unterminated"));

    let week = s
        .stats(&StatsQuery {
            since: ago(7),
            ..hour()
        })
        .unwrap();
    assert_eq!(week.activity.tool_calls, 1);
    assert_eq!(week.runtime_errors, 1);

    assert!(s
        .stats(&StatsQuery {
            until: Some(ago(8)),
            since: ago(7),
            ..hour()
        })
        .is_err());
    assert!(s
        .stats(&StatsQuery {
            buckets: 10_000,
            ..hour()
        })
        .is_err());
}

#[test]
fn files_group_by_path_and_flag_multi_agent_paths() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    submit_all(
        &s,
        vec![
            file_ev("f1", "file.read", "claude-code", "a", "/work/app/src/a.rs"),
            file_ev(
                "f2",
                "file.modified",
                "claude-code",
                "a",
                "/work/app/src/a.rs",
            ),
            file_ev("f3", "file.modified", "codex", "b", "/work/app/src/a.rs"),
            file_ev("f4", "file.created", "codex", "b", "/work/app/docs/new.md"),
        ],
    );
    let q = FilesQuery {
        range: hour(),
        sessions: Vec::new(),
        path_prefix: None,
        multi_agent: false,
        limit: 0,
    };
    let page = s.files(&q).unwrap();
    assert_eq!(page.total_paths, 2);
    let a = page
        .files
        .iter()
        .find(|f| f.path.ends_with("a.rs"))
        .unwrap();
    assert_eq!((a.events, a.reads, a.modified), (3, 1, 2));
    assert_eq!(a.agents, vec!["claude-code", "codex"]);
    assert_eq!(a.sessions, 2);
    assert_eq!(a.last_event_type, "file.modified");
    assert_eq!(a.provenance, "agent_reported");

    let shared = s
        .files(&FilesQuery {
            multi_agent: true,
            ..q.clone()
        })
        .unwrap();
    assert_eq!(shared.files.len(), 1);
    assert_eq!(shared.total_paths, 1);

    let docs = s
        .files(&FilesQuery {
            path_prefix: Some("/work/app/docs/".into()),
            ..q.clone()
        })
        .unwrap();
    assert_eq!(docs.files.len(), 1);
    assert_eq!(docs.files[0].created, 1);

    assert!(s.files(&FilesQuery { limit: 5_000, ..q }).is_err());
}

#[test]
fn session_pages_walk_every_session_once() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let base = Utc::now() - ChronoDuration::minutes(50);
    let mut events = Vec::new();
    for i in 0..7i64 {
        let when = base + ChronoDuration::minutes(i);
        let project = if i % 2 == 0 { "/work/app" } else { "/work/web" };
        events.push(ev(
            &format!("s{i}"),
            "agent.session.started",
            "claude-code",
            &format!("sess-{i}"),
            project,
            when,
        ));
    }
    // Two sessions sharing one timestamp: the key breaks the tie.
    events.push(ev(
        "t1",
        "agent.session.started",
        "codex",
        "tie-a",
        "/work/app",
        base,
    ));
    events.push(ev(
        "t2",
        "agent.session.started",
        "codex",
        "tie-b",
        "/work/app",
        base,
    ));
    submit_all(&s, events);

    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..20 {
        let page = s
            .sessions(&SessionQuery {
                limit: 2,
                cursor: cursor.clone(),
                ..SessionQuery::default()
            })
            .unwrap();
        seen.extend(page.sessions.iter().map(|v| v.session_id.clone()));
        if !page.has_more {
            assert!(page.next_cursor.is_none());
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 9, "{seen:?}");
    assert_eq!(
        seen.iter().collect::<HashSet<_>>().len(),
        9,
        "no duplicates"
    );
    assert_eq!(seen.first().map(String::as_str), Some("claude-code:sess-6"));

    let web = s
        .sessions(&SessionQuery {
            text: Some("WORK/WEB".into()),
            ..SessionQuery::default()
        })
        .unwrap();
    assert_eq!(web.sessions.len(), 3, "case-insensitive substring");

    let early = s
        .sessions(&SessionQuery {
            until: Some(base + ChronoDuration::seconds(30)),
            ..SessionQuery::default()
        })
        .unwrap();
    assert_eq!(early.sessions.len(), 3, "sess-0 and both ties began before");

    for bad in ["nocolon", "x:claude-code:a", "1:"] {
        assert!(
            s.sessions(&SessionQuery {
                cursor: Some(bad.into()),
                ..SessionQuery::default()
            })
            .is_err(),
            "{bad}"
        );
    }
}

#[test]
fn filtered_session_pages_resume_after_the_last_examined_row() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let base = Utc::now() - ChronoDuration::minutes(20);
    let mut events = Vec::new();
    for i in 0..6i64 {
        let when = base + ChronoDuration::minutes(i);
        let sid = format!("e{i}");
        events.push(ev(
            &format!("s{i}"),
            "agent.session.started",
            "claude-code",
            &sid,
            "/work/app",
            when,
        ));
        if i % 2 == 0 {
            events.push(ev(
                &format!("e{i}"),
                "agent.session.ended",
                "claude-code",
                &sid,
                "/work/app",
                when + ChronoDuration::seconds(5),
            ));
        }
    }
    submit_all(&s, events);
    let mut ended = Vec::new();
    let mut cursor = None;
    for _ in 0..10 {
        let page = s
            .sessions(&SessionQuery {
                statuses: vec!["ended".into()],
                limit: 1,
                cursor: cursor.clone(),
                ..SessionQuery::default()
            })
            .unwrap();
        ended.extend(page.sessions.iter().map(|v| v.session_id.clone()));
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(
        ended,
        vec!["claude-code:e4", "claude-code:e2", "claude-code:e0"]
    );
}

#[test]
fn a_failed_command_and_its_failed_tool_call_count_once() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_path(&dir));
    let now = Utc::now() - ChronoDuration::minutes(5);
    let mut tool = ev(
        "t",
        "agent.tool.failed",
        "claude-code",
        "a",
        "/work/app",
        now,
    );
    tool.operation_id = Some("tool:1".into());
    let mut cmd = ev("c", "command.failed", "claude-code", "a", "/work/app", now);
    cmd.operation_id = Some("cmd:1".into());
    cmd.parent_operation_id = Some("tool:1".into());
    // A Codex-style command failure with no failed tool call counts on its own.
    let mut lone = ev("x", "command.failed", "codex", "b", "/work/app", now);
    lone.operation_id = Some("cmd:2".into());
    lone.parent_operation_id = Some("tool:2".into());
    submit_all(&s, vec![tool, cmd, lone]);

    let st = s.stats(&hour()).unwrap();
    assert_eq!(st.activity.tool_failures, 1);
    assert_eq!(st.activity.command_failures, 2);
    assert_eq!(
        st.activity.failures, 2,
        "three failure events, two failed operations"
    );
    assert_eq!(st.buckets.iter().map(|b| b.failures).sum::<u64>(), 2);
    let claude = st.agents.iter().find(|a| a.agent == "claude-code").unwrap();
    assert_eq!(claude.activity.failures, 1);

    let page = s.sessions(&SessionQuery::default()).unwrap();
    let by = |id: &str| {
        page.sessions
            .iter()
            .find(|v| v.session_id == id)
            .unwrap()
            .counts
            .failed_operations
    };
    assert_eq!(by("claude-code:a"), 1);
    assert_eq!(by("codex:b"), 1);
}
