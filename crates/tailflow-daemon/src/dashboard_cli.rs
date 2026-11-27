//! `tailflow-daemon dashboard` and `tailflow-daemon token`.
//!
//! Both talk to a daemon that is already running on this machine, as the
//! same OS user: the API token is read from the daemon's owner-only runtime
//! file (or `$TAILFLOW_TOKEN`), so only someone who could already read the
//! database can sign a browser in.

use crate::auth;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(clap::Args)]
pub struct DashboardArgs {
    /// Print the sign-in link instead of opening a browser
    #[arg(long)]
    no_open: bool,
    /// Sign every browser out of this daemon (ends all dashboard sessions)
    /// instead of opening the dashboard
    #[arg(long, conflicts_with = "no_open")]
    revoke_sessions: bool,
}

/// The token for the daemon on `port`: `$TAILFLOW_TOKEN`, else its runtime
/// file.
fn token_for(port: u16) -> Result<String> {
    if let Some(t) = std::env::var(auth::TOKEN_ENV)
        .ok()
        .filter(|v| !v.is_empty())
    {
        return Ok(t);
    }
    let path = auth::runtime_file_for(port)
        .context("no data directory; set $TAILFLOW_TOKEN or TAILFLOW_DATA_DIR")?;
    let info = auth::read_runtime_file(&path).map_err(|e| {
        anyhow::anyhow!(
            "no running daemon found for port {port} ({e}).\n\
             Start one with `tailflow-daemon`, or pass --port for another daemon."
        )
    })?;
    if info.port != port {
        bail!(
            "{} describes port {}, not {port}",
            path.display(),
            info.port
        );
    }
    Ok(info.token)
}

/// Minimal blocking HTTP/1.1 request to the loopback daemon. Returns the
/// status and body.
fn request(port: u16, method: &str, path: &str, token: &str) -> Result<(u16, String)> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .with_context(|| format!("no TailFlow daemon is listening on 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\nAccept: application/json\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    stream.take(1024 * 1024).read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .context("malformed response from the daemon")?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .context("malformed status line from the daemon")?;
    Ok((status, body.to_string()))
}

fn error_text(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.trim().chars().take(300).collect())
}

pub fn open(port: u16, args: &DashboardArgs) -> Result<()> {
    let token = token_for(port)?;
    if args.revoke_sessions {
        let (status, body) = request(port, "POST", "/api/v1/auth/sessions/revoke", &token)?;
        if status != 200 {
            bail!(
                "revoking sessions failed (HTTP {status}): {}",
                error_text(&body)
            );
        }
        let n = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("revoked").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        eprintln!("Signed out {n} browser session(s). Open the dashboard again with `tailflow-daemon dashboard`.");
        return Ok(());
    }
    let (status, body) = request(port, "POST", "/api/v1/auth/login-links", &token)?;
    if status != 200 {
        bail!(
            "the daemon on port {port} refused to create a sign-in link (HTTP {status}): {}",
            error_text(&body)
        );
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).context("malformed sign-in link response")?;
    let path = v
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|p| p.starts_with("/auth/login/"))
        .context("the daemon returned no sign-in link")?;
    let url = format!("http://127.0.0.1:{port}{path}");
    if args.no_open || !open_browser(&url) {
        println!("{url}");
        eprintln!("One-time sign-in link, valid 5 minutes. Open it in the browser you use for the dashboard.");
    } else {
        eprintln!("Opened the TailFlow dashboard in your browser.");
    }
    Ok(())
}

/// Hand the link to the desktop's URL opener. The link is passed as a
/// single argument, never through a shell.
fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        // `explorer <url>` opens the default browser without `cmd /c start`
        // re-parsing the argument.
        std::process::Command::new("explorer")
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(not(any(unix, windows)))]
    return false;
    #[cfg(any(unix, windows))]
    {
        cmd.arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // `explorer` exits non-zero even when it opened the URL.
        cfg!(windows) || cmd.status().is_ok_and(|s| s.success())
    }
}

pub fn print_token(port: u16) -> Result<()> {
    let token = token_for(port)?;
    // Prove the token belongs to the daemon that is running now; a stale
    // runtime file from a crashed run would otherwise print a dead token.
    let (status, body) = request(port, "GET", "/api/v1/auth/session", &token)?;
    if status != 200 {
        bail!(
            "the token in the runtime file is not accepted by the daemon on port {port} \
             (HTTP {status}): {}",
            error_text(&body)
        );
    }
    println!("{token}");
    Ok(())
}
