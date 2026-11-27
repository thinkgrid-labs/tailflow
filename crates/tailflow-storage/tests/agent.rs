//! Agent capture end to end below the daemon: real hook payloads through
//! the hook bridge (`agent::hook::run`), the spool, the ingestor and
//! storage, then sessions and events read back.

mod common;

use common::*;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tailflow_core::agent::hook;
use tailflow_core::agent::spool::{Spool, MAX_SPOOL_FILES};
use tailflow_core::agent::AgentKind;
use tailflow_core::privacy::{CaptureMode, CapturePolicy, PrivacyConfig};
use tailflow_storage::ingest::SpoolIngestor;
use tailflow_storage::sessions::SessionQuery;
use tailflow_storage::{EventQuery, Storage};

struct Rig {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    spool: Spool,
    policy: CapturePolicy,
}

fn rig() -> Rig {
    rig_with(PrivacyConfig::default())
}

fn rig_with(privacy: PrivacyConfig) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = db_path(&dir);
    let spool = Spool::at(dir.path().join("agents/spool"));
    Rig {
        policy: CapturePolicy::from_config(&privacy).unwrap(),
        _dir: dir,
        db,
        spool,
    }
}

impl Rig {
    fn hook(&self, kind: AgentKind, v: &Value) {
        let text = v.to_string();
        let r = hook::run(kind, &mut text.as_bytes(), Ok(&self.policy), &self.spool);
        assert!(r.outcome.is_ok(), "{r:?}");
    }

    fn ingest(&self, s: &Storage) -> tailflow_storage::ingest::IngestReport {
        let ing = SpoolIngestor::new(self.spool.clone(), s.run_key().to_string());
        let r = ing.drain(s, Duration::from_secs(30));
        s.flush(T).unwrap();
        r
    }
}

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";

fn claude(event: &str, session: &str, extra: Value) -> Value {
    let mut v = json!({"hook_event_name": event, "session_id": session, "cwd": "/work/app",
                       "transcript_path": "/home/u/.claude/projects/x.jsonl", "permission_mode": "default"});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
    }
    v
}

fn a_session(r: &Rig, sid: &str) {
    let k = AgentKind::ClaudeCode;
    r.hook(
        k,
        &claude(
            "SessionStart",
            sid,
            json!({"source":"startup","model":"claude-opus-5-5"}),
        ),
    );
    r.hook(
        k,
        &claude(
            "UserPromptSubmit",
            sid,
            json!({"prompt":"fix the tests, token=ghp_abcdefghijklmnopqrstuvwxyz0123456789"}),
        ),
    );
    r.hook(
        k,
        &claude(
            "PreToolUse",
            sid,
            json!({"tool_name":"Bash","tool_use_id":format!("{sid}-b1"),
        "tool_input":{"command":"cargo test"}}),
        ),
    );
    r.hook(k, &claude("PostToolUseFailure", sid, json!({"tool_name":"Bash","tool_use_id":format!("{sid}-b1"),
        "tool_input":{"command":"cargo test"},"error":"Exit code 101\ntest failed","duration_ms":900})));
    r.hook(
        k,
        &claude(
            "PreToolUse",
            sid,
            json!({"tool_name":"Edit","tool_use_id":format!("{sid}-e1"),
        "tool_input":{"file_path":"/work/app/src/lib.rs","old_string":"a","new_string":"b"}}),
        ),
    );
    r.hook(
        k,
        &claude(
            "PostToolUse",
            sid,
            json!({"tool_name":"Edit","tool_use_id":format!("{sid}-e1"),
        "tool_input":{"file_path":"/work/app/src/lib.rs","old_string":"a","new_string":"b"},
        "tool_response":{"filePath":"/work/app/src/lib.rs"},"duration_ms":12}),
        ),
    );
    r.hook(
        k,
        &claude(
            "Stop",
            sid,
            json!({"stop_hook_active":false,"last_assistant_message":"done"}),
        ),
    );
    r.hook(
        k,
        &claude("SessionEnd", sid, json!({"reason":"prompt_input_exit"})),
    );
}

fn session<'a>(
    page: &'a tailflow_storage::sessions::SessionPage,
    key: &str,
) -> &'a tailflow_storage::sessions::SessionView {
    page.sessions
        .iter()
        .find(|s| s.session_id == key)
        .unwrap_or_else(|| panic!("{key} not listed"))
}

#[test]
fn hook_payloads_become_a_complete_session() {
    let r = rig();
    a_session(&r, S1);
    let s = open(&r.db);
    let rep = r.ingest(&s);
    assert_eq!(rep.files, 8);
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert!(
        r.spool.pending(10).unwrap().is_empty(),
        "ingested records are removed"
    );

    let page = s.sessions(&SessionQuery::default()).unwrap();
    let v = session(&page, &format!("claude-code:{S1}"));
    assert_eq!(v.status, "ended");
    assert_eq!(v.end_reason.as_deref(), Some("prompt_input_exit"));
    assert_eq!(v.start_source.as_deref(), Some("startup"));
    assert_eq!(v.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(
        v.working_directory.as_deref().map(Path::new),
        Some(Path::new("/work/app"))
    );
    assert_eq!(v.counts.tool_calls, 2);
    assert_eq!(v.counts.tool_failures, 1);
    assert_eq!(v.counts.commands, 1);
    assert_eq!(v.counts.command_failures, 1);
    assert_eq!(v.counts.file_events, 1);
    assert_eq!(v.counts.turns, 1);
    assert_eq!(
        v.counts.tokens_total, None,
        "Claude Code reports no tokens: unknown, not 0"
    );
    assert_eq!(v.coverage.history, "complete", "{:?}", v.coverage.reasons);
    assert!(v
        .coverage
        .integration_limits
        .iter()
        .any(|l| l.activity == "tokens.cost"));

    let d = s.session(&v.session_id).unwrap().unwrap();
    assert_eq!(d.programs[0].name, "cargo");
    assert_eq!(
        d.failures
            .iter()
            .find(|f| f.event_type == "command.failed")
            .unwrap()
            .exit_code,
        Some(101)
    );
    assert_eq!(
        Path::new(&d.files[0].path),
        Path::new("/work/app/src/lib.rs")
    );
    assert_eq!(d.files[0].last_event_type, "file.modified");

    // Chronological inspection through the existing events API.
    let ev = s
        .query(&EventQuery {
            sessions: vec![v.session_id.clone()],
            limit: 100,
            ..EventQuery::default()
        })
        .unwrap();
    let types: Vec<&str> = ev
        .events
        .iter()
        .map(|e| e.event.event_type.as_str())
        .collect();
    assert_eq!(types.first(), Some(&"agent.session.started"));
    assert_eq!(types.last(), Some(&"agent.session.ended"));
    assert!(ev.completeness.complete);
    let file = ev
        .events
        .iter()
        .find(|e| e.event.event_type == "file.modified")
        .unwrap();
    assert_eq!(file.event.provenance.as_str(), "agent_reported");
    assert_eq!(file.event.attributes["verified_on_disk"], false);

    // The prompt's secret and text never reached storage.
    s.shutdown(T).unwrap();
    let bytes = disk_bytes(&r.db);
    assert!(!contains(&bytes, "ghp_abcdefghij"));
    assert!(!contains(&bytes, "fix the tests"));
    assert!(
        !contains(&bytes, "test failed"),
        "error text is payload, dropped in metadata mode"
    );
}

#[test]
fn replayed_spool_records_are_deduplicated() {
    let r = rig();
    a_session(&r, S1);
    let copies: Vec<(std::path::PathBuf, Vec<u8>)> = r
        .spool
        .pending(100)
        .unwrap()
        .into_iter()
        .map(|p| {
            let b = std::fs::read(&p).unwrap();
            (p, b)
        })
        .collect();
    let s = open(&r.db);
    r.ingest(&s);
    let before = s.sessions(&SessionQuery::default()).unwrap();
    let events_before = count_rows(&r.db);
    // A daemon that crashed after committing but before deleting its claims
    // re-delivers every record.
    for (p, b) in &copies {
        std::fs::write(p, b).unwrap();
    }
    r.ingest(&s);
    assert_eq!(count_rows(&r.db), events_before, "no duplicate rows");
    let after = s.sessions(&SessionQuery::default()).unwrap();
    assert_eq!(
        session(&after, &format!("claude-code:{S1}")).counts.events,
        session(&before, &format!("claude-code:{S1}")).counts.events,
        "session counts ignore replays"
    );
    assert!(s.health().duplicates_ignored >= 1);
}

#[test]
fn concurrent_sessions_in_one_project_stay_separate() {
    let r = rig();
    // Interleave two Claude sessions and a Codex session in the same cwd.
    let k = AgentKind::ClaudeCode;
    for sid in [S1, S2] {
        r.hook(k, &claude("SessionStart", sid, json!({"source":"startup"})));
    }
    r.hook(AgentKind::Codex, &json!({"hook_event_name":"SessionStart","session_id":S1,
        "cwd":"/work/app","source":"startup","model":"gpt-5.5","permission_mode":"default","transcript_path":null}));
    for i in 0..5 {
        for sid in [S1, S2] {
            r.hook(k, &claude("PostToolUse", sid, json!({"tool_name":"Read",
                "tool_use_id":format!("{sid}-r{i}"),"tool_input":{"file_path":"/work/app/README.md"}})));
        }
    }
    r.hook(AgentKind::Codex, &json!({"hook_event_name":"PostToolUse","session_id":S1,"turn_id":"t1",
        "cwd":"/work/app","tool_name":"apply_patch","tool_use_id":"call_1","model":"m","permission_mode":"default",
        "transcript_path":null,"tool_input":{"command":"*** Begin Patch\n*** Add File: x.rs\n+x\n*** End Patch"},
        "tool_response":{}}));
    r.hook(k, &claude("SessionEnd", S2, json!({"reason":"clear"})));

    let s = open(&r.db);
    r.ingest(&s);
    let page = s.sessions(&SessionQuery::default()).unwrap();
    assert_eq!(
        page.sessions.len(),
        3,
        "same id under two agents = two sessions"
    );
    let c1 = session(&page, &format!("claude-code:{S1}"));
    let c2 = session(&page, &format!("claude-code:{S2}"));
    let x1 = session(&page, &format!("codex:{S1}"));
    assert_eq!(
        (
            c1.counts.tool_calls,
            c2.counts.tool_calls,
            x1.counts.tool_calls
        ),
        (5, 5, 1)
    );
    assert_eq!(c1.status, "active");
    assert_eq!(c2.status, "ended");
    assert_eq!(x1.counts.file_events, 1);
    assert_eq!(
        c1.project_id, c2.project_id,
        "shared project, separate sessions"
    );

    let only_codex = s
        .sessions(&SessionQuery {
            agents: vec!["codex".into()],
            ..SessionQuery::default()
        })
        .unwrap();
    assert_eq!(only_codex.sessions.len(), 1);
    let ended = s
        .sessions(&SessionQuery {
            statuses: vec!["ended".into()],
            ..SessionQuery::default()
        })
        .unwrap();
    assert_eq!(ended.sessions.len(), 1);
    assert!(s
        .sessions(&SessionQuery {
            statuses: vec!["done".into()],
            ..SessionQuery::default()
        })
        .is_err());
}

#[test]
fn partial_and_out_of_order_sessions_are_never_complete() {
    let r = rig();
    let k = AgentKind::ClaudeCode;
    // Joined mid-session: no SessionStart; completion arrives before start.
    r.hook(
        k,
        &claude(
            "PostToolUse",
            S1,
            json!({"tool_name":"Bash","tool_use_id":"x1","tool_input":{"command":"ls"}}),
        ),
    );
    r.hook(
        k,
        &claude(
            "PreToolUse",
            S1,
            json!({"tool_name":"Bash","tool_use_id":"x1","tool_input":{"command":"ls"}}),
        ),
    );
    let s = open(&r.db);
    r.ingest(&s);
    let page = s.sessions(&SessionQuery::default()).unwrap();
    let v = session(&page, &format!("claude-code:{S1}"));
    assert_ne!(v.coverage.history, "complete");
    assert!(v.coverage.reasons.contains(&"start_not_observed"));
    assert_eq!(v.counts.commands, 1, "the started command is counted once");
    assert_eq!(v.started_at, None);

    // A late SessionStart (older timestamp) still sets the earliest start.
    r.hook(k, &claude("SessionStart", S1, json!({"source":"resume"})));
    r.hook(k, &claude("SessionEnd", S1, json!({"reason":"other"})));
    r.ingest(&s);
    let v = s
        .session(&format!("claude-code:{S1}"))
        .unwrap()
        .unwrap()
        .session;
    assert_eq!(v.status, "ended");
    assert_eq!(v.start_source.as_deref(), Some("resume"));
    assert!(v.coverage.start_observed && v.coverage.end_observed);
}

#[test]
fn resume_reopens_an_ended_session() {
    let r = rig();
    let k = AgentKind::ClaudeCode;
    r.hook(k, &claude("SessionStart", S1, json!({"source":"startup"})));
    r.hook(
        k,
        &claude("SessionEnd", S1, json!({"reason":"prompt_input_exit"})),
    );
    std::thread::sleep(Duration::from_millis(5));
    r.hook(k, &claude("SessionStart", S1, json!({"source":"resume"})));
    let s = open(&r.db);
    r.ingest(&s);
    let v = s
        .session(&format!("claude-code:{S1}"))
        .unwrap()
        .unwrap()
        .session;
    assert_eq!(
        v.status, "active",
        "an end before the latest start does not end the session"
    );
    assert_eq!(v.restarts, 1);
    assert_eq!(v.start_source.as_deref(), Some("resume"));
    assert_eq!(v.ended_at, None);
}

#[test]
fn sessions_survive_restart_and_continue() {
    let r = rig();
    let k = AgentKind::ClaudeCode;
    r.hook(k, &claude("SessionStart", S1, json!({"source":"startup"})));
    {
        let s = open(&r.db);
        r.ingest(&s);
        s.shutdown(T).unwrap();
    }
    // Hooks keep firing while no daemon runs.
    r.hook(
        k,
        &claude(
            "PostToolUse",
            S1,
            json!({"tool_name":"Read","tool_use_id":"r1","tool_input":{"file_path":"a"}}),
        ),
    );
    r.hook(k, &claude("SessionEnd", S1, json!({"reason":"other"})));
    assert_eq!(r.spool.stats().pending_files, 2);
    let s = open(&r.db);
    r.ingest(&s);
    let v = s
        .session(&format!("claude-code:{S1}"))
        .unwrap()
        .unwrap()
        .session;
    assert_eq!(v.status, "ended");
    assert_eq!(v.counts.events, 4);
    assert_eq!(v.coverage.history, "complete");
}

#[test]
fn spool_loss_becomes_recorded_gaps() {
    let r = rig();
    let k = AgentKind::ClaudeCode;
    r.hook(k, &claude("SessionStart", S1, json!({"source":"startup"})));
    // Malformed input, a corrupt record and hook-side overflow.
    let bad = hook::run(k, &mut "{oops".as_bytes(), Ok(&r.policy), &r.spool);
    assert!(!bad.rejected.is_empty());
    std::fs::write(
        r.spool.dir().join("1-claude-code-corrupt.json"),
        b"\x00garbage",
    )
    .unwrap();
    r.spool.add_overflow(3, chrono::Utc::now()).unwrap();
    r.hook(k, &claude("SessionEnd", S1, json!({"reason":"other"})));

    let s = open(&r.db);
    let rep = r.ingest(&s);
    assert_eq!((rep.rejected, rep.corrupt_files, rep.overflow), (1, 1, 3));
    let h = s.health();
    assert_eq!(h.events_dropped.agent, 5);
    assert!(h.loss_detected);
    let reasons: Vec<String> = {
        let c = rusqlite::Connection::open(&r.db).unwrap();
        let mut st = c.prepare("SELECT reason FROM gaps ORDER BY id").unwrap();
        st.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    for want in [
        "agent_input_rejected",
        "agent_spool_corrupt",
        "agent_spool_overflow",
    ] {
        assert!(
            reasons.iter().any(|x| x == want),
            "{want} missing from {reasons:?}"
        );
    }
    let v = s
        .session(&format!("claude-code:{S1}"))
        .unwrap()
        .unwrap()
        .session;
    assert_ne!(
        v.coverage.history, "complete",
        "loss during the session is visible on it"
    );
    assert!(v.coverage.reasons.contains(&"loss_recorded"));
}

#[test]
fn excluded_events_are_counted_and_secrets_stay_off_disk_in_redacted_mode() {
    let r = rig_with(PrivacyConfig {
        capture: CaptureMode::Redacted,
        excluded_file_globs: vec!["**/.env".into()],
        ..PrivacyConfig::default()
    });
    let k = AgentKind::ClaudeCode;
    r.hook(k, &claude("PreToolUse", S1, json!({"tool_name":"Bash","tool_use_id":"b",
        "tool_input":{"command":"curl -u admin:hunter22 https://x -H 'Authorization: Bearer abcdefghijklmnop12345'"}})));
    r.hook(
        k,
        &claude(
            "PostToolUse",
            S1,
            json!({"tool_name":"Read","tool_use_id":"r",
        "tool_input":{"file_path":"/work/app/.env"}}),
        ),
    );
    let s = Storage::open(
        &config(&r.db),
        opts(&PrivacyConfig {
            capture: CaptureMode::Redacted,
            ..PrivacyConfig::default()
        }),
    )
    .unwrap();
    r.ingest(&s);
    assert_eq!(
        s.health().events_excluded,
        1,
        "the .env read was excluded by the hook"
    );
    let ev = s
        .query(&EventQuery {
            event_types: vec!["command.started".into()],
            include_payload: true,
            ..EventQuery::default()
        })
        .unwrap();
    let payload = ev.events[0].event.payload.clone().unwrap();
    assert!(
        payload.contains("curl") && payload.contains("[REDACTED"),
        "{payload}"
    );
    s.shutdown(T).unwrap();
    let bytes = disk_bytes(&r.db);
    assert!(!contains(&bytes, "abcdefghijklmnop12345"));
    assert!(!contains(&bytes, "/work/app/.env"));
}

#[test]
fn retention_and_purge_remove_session_rows() {
    let r = rig();
    a_session(&r, S1);
    a_session(&r, S2);
    let s = open(&r.db);
    r.ingest(&s);
    s.shutdown(T).unwrap();
    let report = tailflow_storage::admin::purge(
        &r.db,
        tailflow_storage::admin::PurgeTarget::Session(&format!("claude-code:{S1}")),
    )
    .unwrap();
    assert!(report.deleted > 0);
    let page = tailflow_storage::admin::sessions(&r.db, &SessionQuery::default()).unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].session_id, format!("claude-code:{S2}"));
    tailflow_storage::admin::purge(&r.db, tailflow_storage::admin::PurgeTarget::All).unwrap();
    assert!(
        tailflow_storage::admin::sessions(&r.db, &SessionQuery::default())
            .unwrap()
            .sessions
            .is_empty()
    );
}

#[test]
fn a_saturated_queue_applies_backpressure_instead_of_dropping() {
    let r = rig();
    for i in 0..600 {
        r.hook(
            AgentKind::ClaudeCode,
            &claude(
                "PostToolUse",
                S1,
                json!({"tool_name":"Grep",
            "tool_use_id":format!("q{i}"),"tool_input":{}}),
            ),
        );
    }
    let mut cfg = config(&r.db);
    cfg.queue_capacity = 256;
    let s = Storage::open(&cfg, opts(&PrivacyConfig::default())).unwrap();
    // Stall the writer while the ingestor pushes 600 events into a 256 queue.
    let locker = rusqlite::Connection::open(&r.db).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let ing = SpoolIngestor::new(r.spool.clone(), s.run_key().to_string());
    let report = std::thread::scope(|sc| {
        let h = sc.spawn(|| ing.drain(&s, Duration::from_secs(60)));
        std::thread::sleep(Duration::from_millis(1500));
        locker.execute_batch("COMMIT").unwrap();
        h.join().unwrap()
    });
    s.flush(T).unwrap();
    assert_eq!(report.events_dropped, 0, "{report:?}");
    assert_eq!(
        s.health().events_dropped.queue_full,
        0,
        "the ingestor waited"
    );
    assert_eq!(count_rows(&r.db), 600);
}

#[test]
fn exclusions_and_rejections_are_counted_once_per_record() {
    let r = rig_with(PrivacyConfig {
        excluded_file_globs: vec!["**/.env".into()],
        ..PrivacyConfig::default()
    });
    r.hook(
        AgentKind::ClaudeCode,
        &claude(
            "PostToolUse",
            S1,
            json!({"tool_name":"Read",
        "tool_use_id":"r","tool_input":{"file_path":"/work/app/.env"}}),
        ),
    );
    // A store whose writer has stopped: the event cannot be committed, so
    // the record stays claimed for a retry — the case that re-reads it.
    let stopped = open(&r.db);
    stopped.shutdown(T).unwrap();
    let owner = "fixed-owner";
    let ing = SpoolIngestor::new(r.spool.clone(), owner);
    let first = ing.drain_once(&stopped);
    assert_eq!(first.excluded, 1);
    let kept = r.spool.own_claims(owner).unwrap();
    assert_eq!(kept.len(), 1, "unfinished record kept for replay");
    let rec = tailflow_core::agent::spool::Spool::read(&kept[0]).unwrap();
    assert_eq!(
        rec.excluded, 0,
        "marked counted before its events were submitted"
    );
    assert_eq!(rec.events.len(), 1);
    // The retry against a working store counts the exclusion no second time.
    let s = open(&r.db);
    let again = ing.drain_once(&s);
    assert_eq!(again.excluded, 0);
    assert_eq!(again.events_submitted, 1);
    s.flush(T).unwrap();
    assert_eq!(s.health().events_excluded, 0);
    assert!(r.spool.own_claims(owner).unwrap().is_empty());
}

#[test]
fn late_boundaries_do_not_overwrite_newer_git_facts() {
    let r = rig();
    let s = open(&r.db);
    let mk = |ty: &str, head: &str, branch: &str, ago_s: i64| {
        let mut e = agent_event(
            &format!("agent:claude-code:x:{ty}:{head}"),
            ty,
            "claude-code:g1",
        );
        e.source_type = "agent_hook".into();
        e.agent_name = Some("claude-code".into());
        e.timestamp = chrono::Utc::now() - chrono::Duration::seconds(ago_s);
        e.attributes.insert("git.root".into(), "/work/app".into());
        e.attributes.insert("git.head".into(), head.into());
        e.attributes.insert("git.branch".into(), branch.into());
        e
    };
    // Arrival order: end (newest), then a start, then an *older* start.
    for e in [
        mk("agent.session.ended", "cccc", "feature", 0),
        mk("agent.session.started", "bbbb", "feature", 60),
        mk("agent.session.started", "aaaa", "main", 120),
    ] {
        assert_eq!(s.submit(e), tailflow_storage::Submitted::Enqueued);
    }
    s.flush(T).unwrap();
    let v = s.session("claude-code:g1").unwrap().unwrap().session;
    assert_eq!(
        v.git.head_at_start.as_deref(),
        Some("aaaa"),
        "earliest start wins"
    );
    assert_eq!(v.git.head_at_end.as_deref(), Some("cccc"));
    assert_eq!(
        v.git.branch.as_deref(),
        Some("feature"),
        "latest boundary wins"
    );
}

#[test]
fn concurrent_hooks_and_two_ingestors_lose_and_duplicate_nothing() {
    let r = rig();
    let policy =
        std::sync::Arc::new(CapturePolicy::from_config(&PrivacyConfig::default()).unwrap());
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let spool = r.spool.clone();
            let policy = policy.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    let v = claude(
                        "PostToolUse",
                        S1,
                        json!({"tool_name":"Grep",
                        "tool_use_id":format!("t{t}-{i}"),"tool_input":{"pattern":"x"}}),
                    );
                    let text = v.to_string();
                    hook::run(
                        AgentKind::ClaudeCode,
                        &mut text.as_bytes(),
                        Ok(&policy),
                        &spool,
                    );
                }
            })
        })
        .collect();
    let s1 = open(&r.db);
    let s2 = open(&r.db); // a second daemon on the same database
    let i1 = SpoolIngestor::new(r.spool.clone(), s1.run_key().to_string());
    let i2 = SpoolIngestor::new(r.spool.clone(), s2.run_key().to_string());
    for t in threads {
        t.join().unwrap();
    }
    std::thread::scope(|sc| {
        sc.spawn(|| i1.drain(&s1, Duration::from_secs(30)));
        sc.spawn(|| i2.drain(&s2, Duration::from_secs(30)));
    });
    s1.flush(T).unwrap();
    s2.flush(T).unwrap();
    assert_eq!(count_rows(&r.db), 400);
    let v = s1
        .session(&format!("claude-code:{S1}"))
        .unwrap()
        .unwrap()
        .session;
    assert_eq!(v.counts.tool_calls, 400);
    assert!(r.spool.pending(10).unwrap().is_empty());
    assert_eq!(r.spool.stats().claimed_files, 0);
    let _ = MAX_SPOOL_FILES;
    let _: &Path = r.spool.dir();
}
