//! End-to-end agent capture with the real daemon binary: hook calls spooled
//! while no daemon runs are ingested at start, live hook calls are ingested
//! within a second, sessions are served over HTTP, SIGTERM stops cleanly
//! with the ingest thread running, and the sessions CLI works offline.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tailflow_core::agent::{hook, spool::Spool, AgentKind};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};

/// Fixed API token for the spawned daemons (`$TAILFLOW_TOKEN`).
const TOKEN: &str = "e2e-test-token-0123456789abcdef0123456789";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(port: u16, path: &str) -> Option<Value> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
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
        }
        assert!(Instant::now() < deadline, "timed out waiting on {path}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn hook_call(spool: &Spool, v: &Value) {
    let policy = CapturePolicy::from_config(&PrivacyConfig::default()).unwrap();
    let text = v.to_string();
    hook::run(
        AgentKind::ClaudeCode,
        &mut text.as_bytes(),
        Ok(&policy),
        spool,
    );
}

fn daemon(data: &Path, port: u16, cwd: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(["--agents", "--port", &port.to_string()])
        .env("TAILFLOW_DATA_DIR", data)
        .env("TAILFLOW_TOKEN", TOKEN)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Kills the daemon if the test panics, so no orphan outlives it.
struct Guard(Option<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn sigterm_and_wait(mut guard: Guard) -> Duration {
    let mut child = guard.0.take().unwrap();
    let started = Instant::now();
    let pid = child.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "daemon exited with {status}");
            return started.elapsed();
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "daemon did not stop on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn agent_only_daemon_ingests_spooled_and_live_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let spool = Spool::at(data.join("agents/spool"));
    let sid = "e2e-0001";
    let base = json!({"session_id": sid, "cwd": dir.path().display().to_string()});
    let with = |extra: Value| {
        let mut v = base.clone();
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    // Before any daemon runs.
    hook_call(
        &spool,
        &with(json!({"hook_event_name":"SessionStart","source":"startup"})),
    );
    hook_call(
        &spool,
        &with(
            json!({"hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"t1",
        "tool_input":{"command":"npm test"}}),
        ),
    );
    assert_eq!(spool.stats().pending_files, 2);

    let port = free_port();
    let child = Guard(Some(daemon(&data, port, dir.path())));
    let key = format!("claude-code:{sid}");
    wait_for(port, "/api/v1/sessions", Duration::from_secs(15), |v| {
        v["sessions"][0]["counts"]["events"].as_u64() == Some(3)
    });
    assert_eq!(
        spool.stats().pending_files,
        0,
        "spooled records were ingested at start"
    );

    // Live: measure hook-to-queryable latency.
    let t = Instant::now();
    hook_call(
        &spool,
        &with(
            json!({"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":"t1",
        "tool_input":{"command":"npm test"},"error":"Exit code 1","duration_ms":4200}),
        ),
    );
    hook_call(
        &spool,
        &with(json!({"hook_event_name":"SessionEnd","reason":"prompt_input_exit"})),
    );
    let v = wait_for(
        port,
        &format!("/api/v1/sessions/{key}"),
        Duration::from_secs(40),
        |v| v["status"] == "ended",
    );
    let latency = t.elapsed();
    eprintln!("live hook → queryable session: {latency:?}");
    assert_eq!(v["counts"]["command_failures"], 1);
    assert_eq!(v["coverage"]["history"], "complete");
    assert_eq!(v["failures"][0]["exit_code"], 1);
    let ev = get(port, &format!("/api/v1/events?session={key}&limit=50")).unwrap();
    // start, tool + command started, tool + command failed, end.
    assert_eq!(ev["events"].as_array().unwrap().len(), 6);
    assert!(
        latency < Duration::from_secs(5),
        "ingestion is event-driven, not the 30 s rescan"
    );
    let st = get(port, "/api/v1/agents/status").unwrap();
    assert_eq!(st["ingest"]["enabled"], true);
    assert!(st["ingest"]["files_ingested"].as_u64().unwrap() >= 4);

    let stop = sigterm_and_wait(child);
    eprintln!("SIGTERM → exit: {stop:?}");
    assert_eq!(spool.stats().claimed_files, 0, "no claims left behind");

    // Offline: hooks after the daemon stopped, then the CLI ingests and lists.
    hook_call(
        &spool,
        &json!({"hook_event_name":"SessionStart","session_id":"e2e-0002",
        "cwd": dir.path().display().to_string(),"source":"startup"}),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(["sessions", "--json"])
        .env("TAILFLOW_DATA_DIR", &data)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let page: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&"claude-code:e2e-0002"));
    let human = Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(["session", "e2e-0001", "--events"])
        .env("TAILFLOW_DATA_DIR", &data)
        .current_dir(dir.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    assert!(text.contains("command `npm` failed (exit 1)"), "{text}");
    assert!(text.contains("history    complete"), "{text}");
}
