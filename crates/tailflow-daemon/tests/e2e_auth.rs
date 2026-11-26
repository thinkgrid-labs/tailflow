//! Local API access control against the real daemon binary: the runtime
//! file, `tailflow-daemon token`, `tailflow-daemon dashboard`, sign-in link
//! redemption, credential rotation on restart and cleanup on shutdown.

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

/// Raw request; returns (status, headers, body).
fn request(
    port: u16,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> Option<(u16, String, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let (head, body) = buf.split_once("\r\n\r\n")?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, head.to_string(), body.to_string()))
}

fn spawn(data: &Path, port: u16, cwd: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(["--agents", "--port", &port.to_string()])
        .env("TAILFLOW_DATA_DIR", data)
        .env_remove("TAILFLOW_TOKEN")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn cli(data: &Path, port: u16, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(args)
        .args(["--port", &port.to_string()])
        .env("TAILFLOW_DATA_DIR", data)
        .env_remove("TAILFLOW_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Another workspace binary next to the daemon (built by `cargo test --all`).
fn sibling_bin(name: &str) -> Option<std::path::PathBuf> {
    let daemon = std::path::PathBuf::from(env!("CARGO_BIN_EXE_tailflow-daemon"));
    let p = daemon
        .parent()?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    p.exists().then_some(p)
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

fn runtime_file(data: &Path, port: u16) -> std::path::PathBuf {
    data.join("run").join(format!("daemon-{port}.json"))
}

fn wait_for_runtime_file(data: &Path, port: u16) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(runtime_file(data, port)) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if request(port, "GET", "/health", &[]).is_some_and(|r| r.0 == 200) {
                    return v;
                }
            }
        }
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn stop(child: &mut Child) {
    let ok = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn local_clients_authenticate_through_the_runtime_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let port = free_port();
    let mut guard = Guard(Some(spawn(&data, port, dir.path())));
    let info = wait_for_runtime_file(&data, port);
    let token = info["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    assert_eq!(info["port"], port);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(runtime_file(&data, port))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "runtime file is owner-only");
    }

    // Without the token: refused. With it: served.
    let (status, _, _) = request(port, "GET", "/api/v1/sessions", &[]).unwrap();
    assert_eq!(status, 401);
    let auth = format!("Bearer {token}");
    let (status, _, body) =
        request(port, "GET", "/api/v1/sessions", &[("Authorization", &auth)]).unwrap();
    assert_eq!(status, 200, "{body}");

    // The agent CLI finds the token by itself (same user, same data dir).
    if let Some(logs) = sibling_bin("tailflow-logs") {
        let run = |data_dir: &Path| {
            Command::new(&logs)
                .args(["sources", "--json"])
                .env("TAILFLOW_URL", format!("http://127.0.0.1:{port}"))
                .env("TAILFLOW_DATA_DIR", data_dir)
                .env_remove("TAILFLOW_TOKEN")
                .output()
                .unwrap()
        };
        let out = run(&data);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Another user / data dir: no token, a distinct exit code and a hint.
        let out = run(&dir.path().join("elsewhere"));
        assert_eq!(
            out.status.code(),
            Some(4),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("TAILFLOW_TOKEN"));
    } else {
        eprintln!("tailflow-logs not built; skipped the agent CLI check");
    }

    // `tailflow-daemon token` prints exactly the running daemon's token.
    let out = cli(&data, port, &["token"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), token);

    // `tailflow-daemon dashboard --no-open` prints a one-time link.
    let out = cli(&data, port, &["dashboard", "--no-open"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let prefix = format!("http://127.0.0.1:{port}/auth/login/");
    let path = url
        .strip_prefix(&format!("http://127.0.0.1:{port}"))
        .unwrap_or_else(|| panic!("{url} does not start with {prefix}"))
        .to_string();
    let (status, head, _) = request(port, "GET", &path, &[("Sec-Fetch-Site", "none")]).unwrap();
    assert_eq!(status, 303, "{head}");
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .expect("session cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(
        cookie.starts_with(&format!("tailflow_session_{port}=")),
        "{cookie}"
    );
    let (status, _, _) = request(
        port,
        "GET",
        "/api/v1/sessions",
        &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
    )
    .unwrap();
    assert_eq!(status, 200);
    // The link worked once.
    let (status, _, _) = request(port, "GET", &path, &[("Sec-Fetch-Site", "none")]).unwrap();
    assert_eq!(status, 401);

    #[cfg(unix)]
    {
        // Clean shutdown removes the runtime file.
        let mut child = guard.0.take().unwrap();
        stop(&mut child);
        assert!(
            !runtime_file(&data, port).exists(),
            "runtime file removed on clean shutdown"
        );

        // A restart rotates the API token; the browser session survives
        // (open dashboards reconnect) until it is revoked.
        let mut guard2 = Guard(Some(spawn(&data, port, dir.path())));
        let info2 = wait_for_runtime_file(&data, port);
        assert_ne!(info2["token"], info["token"], "token rotates per run");
        let (status, _, _) =
            request(port, "GET", "/api/v1/sessions", &[("Authorization", &auth)]).unwrap();
        assert_eq!(status, 401, "old token refused");
        let session_req = || {
            request(
                port,
                "GET",
                "/api/v1/sessions",
                &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
            )
            .unwrap()
        };
        assert_eq!(session_req().0, 200, "browser session survives the restart");
        let out = cli(&data, port, &["dashboard", "--revoke-sessions"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let (status, _, body) = session_req();
        assert_eq!(status, 401, "revoked session refused");
        assert!(body.contains("session_expired"), "{body}");

        // After a hard kill the file is stale; `token` notices instead of
        // printing a dead credential.
        let mut child = guard2.0.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(runtime_file(&data, port).exists());
        let out = cli(&data, port, &["token"]);
        assert!(!out.status.success(), "stale token must not be printed");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn an_invalid_env_token_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tailflow-daemon"))
        .args(["--agents", "--port", &free_port().to_string()])
        .env("TAILFLOW_DATA_DIR", dir.path().join("data"))
        .env("TAILFLOW_TOKEN", "short")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("TAILFLOW_TOKEN"), "{stderr}");
}
