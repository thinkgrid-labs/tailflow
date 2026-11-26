//! Filesystem observation and Git evidence (M4-C, M4-D) with the real
//! daemon, a real Git repository and real agent hook calls.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tailflow_core::agent::{hook, spool::Spool, AgentKind};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};

const TOKEN: &str = "e2e-observe-token-0123456789abcdef0123456";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(port: u16, path: &str) -> Option<Value> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    serde_json::from_str(buf.split_once("\r\n\r\n")?.1).ok()
}

fn wait_for(port: u16, path: &str, timeout: Duration, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = get(port, path) {
            if ok(&v) {
                return v;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting on {path}: {v}");
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting on {path}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Guard(Option<Child>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _k = c.kill();
            let _w = c.wait();
        }
    }
}

fn daemon(data: &Path, port: u16, cwd: &Path, observe: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(["--agents", "--port", &port.to_string(), "--observe"])
        .arg(observe)
        .env("TAILFLOW_DATA_DIR", data)
        .env("TAILFLOW_TOKEN", TOKEN)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

fn hook_call(spool: &Spool, v: &Value) {
    let policy = CapturePolicy::from_config(&PrivacyConfig::default()).unwrap();
    hook::run(
        AgentKind::ClaudeCode,
        &mut v.to_string().as_bytes(),
        Ok(&policy),
        spool,
    );
}

fn statuses(page: &Value, suffix: &str) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["path"].as_str().unwrap().ends_with(suffix))
        .map(|i| i["status"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn reported_observed_and_git_evidence_end_to_end() {
    if tailflow_core::observe::git::find_git().is_none() {
        eprintln!("git not installed; skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for f in ["a.rs", "b.rs", "pre.rs", "human.rs"] {
        std::fs::write(repo.join(f), "v1\n").unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // Dirty before any session: must never be credited to it.
    std::fs::write(repo.join("pre.rs"), "pre-existing edit\n").unwrap();
    let index_mtime = std::fs::metadata(repo.join(".git/index"))
        .unwrap()
        .modified()
        .unwrap();

    let port = free_port();
    let _d = Guard(Some(daemon(&data, port, dir.path(), &repo)));
    wait_for(
        port,
        "/api/v1/observe/status",
        Duration::from_secs(30),
        |v| v["roots"][0]["state"] == "observing" && v["repos"][0]["snapshots"].as_u64() >= Some(1),
    );
    let spool = Spool::at(data.join("agents/spool"));
    let cwd = repo.display().to_string();
    let call = |extra: Value| {
        let mut v = json!({"session_id": "e2e-obs", "cwd": cwd});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        hook_call(&spool, &v);
    };
    call(json!({"hook_event_name": "SessionStart", "source": "startup"}));
    let sid = "claude-code:e2e-obs";
    // The session-start snapshot.
    wait_for(
        port,
        &format!("/api/v1/events?type=git.snapshot&session={sid}"),
        Duration::from_secs(20),
        |v| v["events"].as_array().is_some_and(|e| !e.is_empty()),
    );
    std::thread::sleep(Duration::from_millis(500));

    // The agent edits a.rs and reports it.
    std::fs::write(repo.join("a.rs"), "v2 by agent\n").unwrap();
    let a = repo.join("a.rs").display().to_string();
    call(
        json!({"hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_use_id": "e1",
        "tool_input": {"file_path": a}, "tool_response": {}}),
    );
    // The agent reports an edit of b.rs that never reached the disk.
    let b = repo.join("b.rs").display().to_string();
    call(
        json!({"hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_use_id": "e2",
        "tool_input": {"file_path": b}, "tool_response": {}}),
    );
    // A person edits human.rs; no hook.
    std::fs::write(repo.join("human.rs"), "v2 by a person\n").unwrap();
    std::thread::sleep(Duration::from_secs(1));
    call(json!({"hook_event_name": "SessionEnd", "reason": "other"}));

    let ev = wait_for(
        port,
        "/api/v1/evidence/files?since=10m",
        Duration::from_secs(30),
        |v| {
            !statuses(v, "/human.rs").is_empty()
                && !statuses(v, "/a.rs").is_empty()
                && !statuses(v, "/b.rs").is_empty()
        },
    );
    assert_eq!(statuses(&ev, "/a.rs"), ["matched_observation"], "{ev}");
    assert_eq!(statuses(&ev, "/b.rs"), ["reported_only"], "{ev}");
    assert_eq!(statuses(&ev, "/human.rs"), ["filesystem_observed"], "{ev}");
    assert_eq!(ev["coverage"].as_array().unwrap().len(), 1);

    let changes = wait_for(
        port,
        &format!("/api/v1/sessions/{sid}/changes"),
        Duration::from_secs(30),
        |v| v["available"] == true && v["after"]["trigger"] == "session_end",
    );
    let by_path = |p: &str| {
        changes["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == p)
            .cloned()
    };
    let a_change = by_path("a.rs").unwrap_or_else(|| panic!("{changes}"));
    assert_eq!(a_change["evidence"], "reported_by_this_session");
    assert!(a_change["observations"].as_u64().unwrap() >= 1);
    let h = by_path("human.rs").unwrap();
    assert_eq!(h["evidence"], "unreported");
    assert!(by_path("b.rs").is_none(), "b.rs never changed on disk");
    assert!(
        changes["pre_existing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "pre.rs"),
        "pre-existing dirty file is not credited: {changes}"
    );
    assert_eq!(changes["baseline"]["trigger"], "session_start");
    // Git was only read.
    assert_eq!(
        std::fs::metadata(repo.join(".git/index"))
            .unwrap()
            .modified()
            .unwrap(),
        index_mtime
    );
    assert!(!repo.join(".git/index.lock").exists());

    // A second daemon sharing the database does not observe the same root.
    let port2 = free_port();
    let _d2 = Guard(Some(daemon(&data, port2, dir.path(), &repo)));
    let st = wait_for(
        port2,
        "/api/v1/observe/status",
        Duration::from_secs(30),
        |v| v["roots"][0]["state"] != "starting",
    );
    assert_eq!(st["roots"][0]["state"], "waiting", "{st}");
}
