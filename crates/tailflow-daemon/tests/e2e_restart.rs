//! End-to-end: the real daemon binary, a real process source, `kill -9`, and
//! restarts. Unix-only (signals).
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(port: u16, path: &str) -> Option<serde_json::Value> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let body = buf.split_once("\r\n\r\n")?.1;
    serde_json::from_str(body).ok()
}

fn wait_for(
    port: u16,
    path: &str,
    timeout: Duration,
    ok: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = get(port, path) {
            if ok(&v) {
                return v;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting on {path}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn(config: &Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .arg("--config")
        .arg(config)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn events(port: u16) -> u64 {
    get(port, "/api/v1/storage/status")
        .and_then(|v| v["db"]["events"].as_u64())
        .unwrap_or(0)
}

#[test]
fn history_survives_kill_9_and_the_loss_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.db");
    let config = dir.path().join("tailflow.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[[sources.process]]
label = "gen"
cmd = "i=0; while [ $i -lt 3000 ]; do echo \"line $i password=hunter2\"; i=$((i+1)); sleep 0.005; done"

[storage]
path = "{}"
batch_max_delay_ms = 20
"#,
            db.display()
        ),
    )
    .unwrap();

    // Run 1: collect, then kill -9 mid-stream.
    let port = free_port();
    let mut d1 = spawn(&config, port);
    wait_for(
        port,
        "/api/v1/storage/status",
        Duration::from_secs(30),
        |v| v["db"]["events"].as_u64().unwrap_or(0) > 100,
    );
    d1.kill().unwrap(); // SIGKILL
    d1.wait().unwrap();

    // Run 2: history from run 1 is there, and the unclean exit is a gap.
    let port = free_port();
    let restart = Instant::now();
    let mut d2 = spawn(&config, port);
    let page = wait_for(
        port,
        "/api/v1/events?type=runtime.log&limit=1000",
        Duration::from_secs(30),
        |v| v["events"].as_array().is_some_and(|e| !e.is_empty()),
    );
    let recovery = restart.elapsed();
    eprintln!("restart-to-first-query: {recovery:?}");
    let first = &page["events"][0];
    assert!(
        first["message"].as_str().unwrap().starts_with("line "),
        "run 1 history was recovered: {first}"
    );
    assert!(
        !page.to_string().contains("hunter2"),
        "redacted before disk, in both runs"
    );
    let gaps = wait_for(
        port,
        "/api/v1/events?limit=1000",
        Duration::from_secs(10),
        |v| v["completeness"]["complete"] == false,
    );
    assert!(gaps["completeness"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["reason"] == "unclean_shutdown" && g["count"].is_null()));
    let status = get(port, "/api/v1/storage/status").unwrap();
    assert_eq!(status["recovered_unclean_runs"], 1);

    // Run 2 keeps ingesting, then stops cleanly on SIGTERM.
    let before = events(port);
    wait_for(
        port,
        "/api/v1/storage/status",
        Duration::from_secs(30),
        |v| v["db"]["events"].as_u64().unwrap_or(0) > before + 50,
    );
    let killed = Command::new("kill")
        .args(["-TERM", &d2.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if d2.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stderr = String::new();
    d2.stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("previous run") && stderr.contains("without a clean shutdown"),
        "the operator is told about the gap: {stderr}"
    );

    // Run 3: a clean SIGTERM is not reported as loss.
    let port = free_port();
    let mut d3 = spawn(&config, port);
    let status = wait_for(
        port,
        "/api/v1/storage/status",
        Duration::from_secs(30),
        |v| v["state"] == "ok",
    );
    assert_eq!(
        status["recovered_unclean_runs"], 0,
        "SIGTERM shut down cleanly"
    );
    d3.kill().unwrap();
    d3.wait().unwrap();
}

#[test]
fn unusable_database_degrades_to_memory_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.db");
    std::fs::write(&db, b"not a database at all".repeat(64)).unwrap();
    let original = std::fs::read(&db).unwrap();
    let config = dir.path().join("tailflow.toml");
    std::fs::write(
        &config,
        format!(
            "[[sources.process]]\nlabel = \"gen\"\ncmd = \"echo hello; sleep 30\"\n\n[storage]\npath = \"{}\"\n",
            db.display()
        ),
    )
    .unwrap();
    let port = free_port();
    let mut d = spawn(&config, port);
    let q = wait_for(port, "/api/query", Duration::from_secs(30), |v| {
        v["records"].as_array().is_some_and(|r| !r.is_empty())
    });
    assert_eq!(q["records"][0]["payload"], "hello", "live logs unaffected");
    let status = get(port, "/api/v1/storage/status").unwrap();
    assert_eq!(status["state"], "unavailable");
    assert_eq!(status["kind"], "not_a_database");
    d.kill().unwrap();
    d.wait().unwrap();
    assert_eq!(
        std::fs::read(&db).unwrap(),
        original,
        "never reset or rewritten"
    );
}

#[test]
fn invalid_privacy_config_fails_startup_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("tailflow.toml");
    std::fs::write(
        &config,
        "[[sources.process]]\nlabel = \"gen\"\ncmd = \"echo hi\"\n\n[[privacy.redaction_rules]]\nname = \"broken\"\npattern = \"([unclosed\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .arg("--config")
        .arg(&config)
        .arg("--port")
        .arg(free_port().to_string())
        .arg("--db")
        .arg(dir.path().join("e.db"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("broken"), "names the bad rule: {stderr}");
}

#[test]
fn storage_cli_status_check_and_guarded_purge() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.db");
    let config = dir.path().join("tailflow.toml");
    std::fs::write(
        &config,
        format!(
            "[[sources.process]]\nlabel = \"gen\"\ncmd = \"echo one; echo two; sleep 30\"\n\n[storage]\npath = \"{}\"\nbatch_max_delay_ms = 10\n",
            db.display()
        ),
    )
    .unwrap();
    let port = free_port();
    let mut d = spawn(&config, port);
    wait_for(
        port,
        "/api/v1/storage/status",
        Duration::from_secs(30),
        |v| v["db"]["events"].as_u64().unwrap_or(0) >= 2,
    );
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
            .args(args)
            .arg("--db")
            .arg(&db)
            .output()
            .unwrap()
    };
    let out = cli(&["storage", "status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["events"].as_u64().unwrap() >= 2);
    let out = cli(&["storage", "check"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Without --yes nothing is deleted.
    let out = cli(&["storage", "purge", "--all"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(events(port) >= 2);
    // With --yes, while the daemon is running.
    let out = cli(&["storage", "purge", "--all", "--yes"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(events(port), 0);
    let page = get(port, "/api/v1/events").unwrap();
    assert_eq!(page["completeness"]["retention_truncated"], true);
    d.kill().unwrap();
    d.wait().unwrap();
}

#[test]
fn sigterm_stops_cleanly_while_an_sse_client_is_connected() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.db");
    let config = dir.path().join("tailflow.toml");
    std::fs::write(
        &config,
        format!(
            "[[sources.process]]\nlabel = \"gen\"\ncmd = \"echo hello; sleep 30\"\n\n[storage]\npath = \"{}\"\n",
            db.display()
        ),
    )
    .unwrap();
    let port = free_port();
    let mut daemon = spawn(&config, port);
    wait_for(
        port,
        "/api/v1/storage/status",
        Duration::from_secs(20),
        |v| v["db"]["events"].as_u64().unwrap_or(0) >= 1,
    );

    // A live-stream client (a dashboard tab, `curl /events`) stays connected.
    let mut sse = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        sse,
        "GET /events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    sse.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut head = [0u8; 64];
    let n = sse.read(&mut head).unwrap();
    assert!(String::from_utf8_lossy(&head[..n]).starts_with("HTTP/1.1 200"));

    let started = Instant::now();
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(daemon.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let exit = loop {
        if let Some(exit) = daemon.try_wait().unwrap() {
            break exit;
        }
        if started.elapsed() > Duration::from_secs(15) {
            let _ = daemon.kill();
            panic!("the daemon ignored SIGTERM while an SSE client was connected");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(exit.success(), "clean exit, got {exit:?}");
    drop(sse);

    // The stop was clean: the next start finds no unclean run to recover.
    let port = free_port();
    let mut daemon = spawn(&config, port);
    let status = wait_for(
        port,
        "/api/v1/storage/status",
        Duration::from_secs(20),
        |v| v["recovered_unclean_runs"].is_number(),
    );
    assert_eq!(status["recovered_unclean_runs"], 0, "{status}");
    let _ = daemon.kill();
    let _ = daemon.wait();
}
