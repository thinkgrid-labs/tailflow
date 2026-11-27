//! Live agent stream (M4-B) across real daemon processes: events committed
//! by *another* daemon sharing the per-user database reach a dashboard
//! connected to this one; a stream resumes after a daemon restart from its
//! persistent cursor without duplicates; commit-to-delivery latency.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tailflow_core::agent::{hook, spool::Spool, AgentKind};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};

const TOKEN: &str = "e2e-stream-token-0123456789abcdef01234567";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Guard(Option<Child>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _killed = c.kill();
            let _reaped = c.wait();
        }
    }
}

/// `ingest`: whether this daemon moves spooled hook events into storage.
fn daemon(data: &Path, port: u16, cwd: &Path, ingest: bool) -> Child {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"));
    c.args(["--port", &port.to_string()]);
    if ingest {
        c.arg("--agents");
    } else {
        c.arg("--no-agents");
    }
    c.env("TAILFLOW_DATA_DIR", data)
        .env("TAILFLOW_TOKEN", TOKEN)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_up(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "daemon on {port} did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A raw SSE client.
struct Sse {
    r: BufReader<TcpStream>,
}

#[derive(Debug)]
struct Frame {
    event: String,
    id: Option<u64>,
    data: String,
}

impl Sse {
    fn open(port: u16, query: &str) -> Self {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        write!(
            s,
            "GET /api/v1/stream/agent{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Authorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status).unwrap();
        assert!(status.contains(" 200 "), "{status}");
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            if l == "\r\n" {
                break;
            }
        }
        Self { r }
    }

    /// Next named frame (chunked-encoding size lines and comments skipped).
    fn next(&mut self) -> Option<Frame> {
        let mut f = Frame {
            event: String::new(),
            id: None,
            data: String::new(),
        };
        loop {
            let mut l = String::new();
            if self.r.read_line(&mut l).ok()? == 0 {
                return None;
            }
            let l = l.trim_end_matches(['\r', '\n']);
            if let Some(v) = l.strip_prefix("event:") {
                f.event = v.trim().into();
            } else if let Some(v) = l.strip_prefix("id:") {
                f.id = v.trim().parse().ok();
            } else if let Some(v) = l.strip_prefix("data:") {
                f.data.push_str(v.trim_start());
            } else if l.is_empty() && !f.event.is_empty() {
                return Some(f);
            }
        }
    }

    fn until(&mut self, event: &str) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(f) = self.next() {
            let done = f.event == event;
            out.push(f);
            if done {
                break;
            }
        }
        out
    }
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

fn tool(sid: &str, cwd: &Path, id: &str) -> Value {
    json!({"session_id": sid, "cwd": cwd.display().to_string(),
        "hook_event_name":"PreToolUse","tool_name":"Read","tool_use_id": id,
        "tool_input":{"file_path": cwd.join("a.rs").display().to_string()}})
}

#[test]
fn events_committed_by_another_daemon_stream_live_and_resume_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(dir.path().join("viewer")).unwrap();
    // The viewer daemon needs a source because it does not record agents.
    std::fs::write(
        dir.path().join("viewer/tailflow.toml"),
        "[[sources.process]]\nlabel = \"idle\"\ncmd = \"sleep 600\"\n",
    )
    .unwrap();
    let spool = Spool::at(data.join("agents/spool"));
    let (viewer_port, ingest_port) = (free_port(), free_port());
    let mut viewer = Guard(Some(daemon(
        &data,
        viewer_port,
        &dir.path().join("viewer"),
        false,
    )));
    let _ingester = Guard(Some(daemon(&data, ingest_port, dir.path(), true)));
    wait_up(viewer_port);
    wait_up(ingest_port);

    let mut sse = Sse::open(viewer_port, "");
    sse.until("ready");

    // Hook calls → spool → the *other* daemon commits → this stream.
    let mut latencies = Vec::new();
    let mut last = 0u64;
    for i in 0..5 {
        let t0 = Instant::now();
        hook_call(&spool, &tool("multi", dir.path(), &format!("t{i}")));
        let f = loop {
            let f = sse.next().expect("stream open");
            if f.event == "agent" {
                break f;
            }
        };
        latencies.push(t0.elapsed());
        let seq = f.id.unwrap();
        assert!(seq > last, "seq order");
        last = seq;
        let v: Value = serde_json::from_str(&f.data).unwrap();
        assert_eq!(v["event_type"], "agent.tool.started");
    }
    eprintln!("hook→stream latency via another daemon: {latencies:?}");
    // Spool watcher + batch + cross-process poll: well under 2 s each.
    assert!(
        latencies.iter().all(|d| *d < Duration::from_secs(3)),
        "{latencies:?}"
    );

    // Restart the viewer; reconnect with the last delivered seq.
    drop(sse);
    let mut child = viewer.0.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    // Events committed while the viewer is down.
    for i in 5..8 {
        hook_call(&spool, &tool("multi", dir.path(), &format!("t{i}")));
    }
    std::thread::sleep(Duration::from_millis(1500));
    let _viewer2 = Guard(Some(daemon(
        &data,
        viewer_port,
        &dir.path().join("viewer"),
        false,
    )));
    wait_up(viewer_port);
    let mut sse = Sse::open(viewer_port, &format!("?after={last}"));
    let frames = sse.until("ready");
    let resumed: Vec<u64> = frames
        .iter()
        .filter(|f| f.event == "agent" && f.data.contains("\"agent.tool.started\""))
        .filter_map(|f| f.id)
        .collect();
    assert_eq!(resumed.len(), 3, "exactly the missed events: {frames:?}");
    // The viewer was killed (not stopped): its restart records an unclean
    // shutdown, and the stream says so instead of implying completeness.
    assert!(
        frames
            .iter()
            .any(|f| f.event == "gap" && f.data.contains("unclean_shutdown")),
        "{frames:?}"
    );
    assert!(resumed.iter().all(|s| *s > last));
    assert!(resumed.windows(2).all(|w| w[0] < w[1]));
}
