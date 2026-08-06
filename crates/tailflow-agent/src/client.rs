//! Minimal HTTP client for the local TailFlow daemon.
//!
//! Deliberately hand-rolled rather than pulling in a general HTTP stack: every
//! request is a plain GET to `127.0.0.1` with no TLS, no redirects, and no
//! auth. A full client would add a large dependency tree to two binaries whose
//! entire job is to shuttle JSON across a loopback socket.

use serde_json::Value;
use std::fmt;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_URL: &str = "http://127.0.0.1:7878";
/// Env var override, so an agent inherits the right port without a flag.
pub const URL_ENV: &str = "TAILFLOW_URL";

#[derive(Debug)]
pub enum ClientError {
    /// Nothing is listening. By far the most common failure, and the only one
    /// with an obvious fix — so it gets its own variant and its own message.
    NotRunning {
        url: String,
    },
    Http {
        status: u16,
        message: String,
    },
    Io(String),
    Parse(String),
    Timeout {
        after: Duration,
    },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotRunning { url } => write!(
                f,
                "No TailFlow daemon is listening at {url}.\n\
                 Start one from your project root (it reads tailflow.toml):\n\
                 \n    tailflow-daemon\n\n\
                 Or point at a different port with {URL_ENV}=http://127.0.0.1:PORT."
            ),
            ClientError::Http { status, message } => {
                write!(f, "daemon returned HTTP {status}: {message}")
            }
            ClientError::Io(e) => write!(f, "transport error talking to the daemon: {e}"),
            ClientError::Parse(e) => write!(f, "malformed response from the daemon: {e}"),
            ClientError::Timeout { after } => {
                write!(f, "daemon did not respond within {:?}", after)
            }
        }
    }
}

impl std::error::Error for ClientError {}

pub struct DaemonClient {
    /// `host:port`, already stripped of scheme and trailing slash.
    authority: String,
    display_url: String,
}

impl DaemonClient {
    /// Resolve the daemon address from an explicit override, then `$TAILFLOW_URL`,
    /// then the default port.
    pub fn resolve(explicit: Option<&str>) -> Self {
        let raw = explicit
            .map(str::to_string)
            .or_else(|| std::env::var(URL_ENV).ok())
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        Self::new(&raw)
    }

    pub fn new(url: &str) -> Self {
        let trimmed = url.trim().trim_end_matches('/');
        let without_scheme = trimmed
            .strip_prefix("http://")
            .or_else(|| trimmed.strip_prefix("https://"))
            .unwrap_or(trimmed);
        // A bare port ("7878") is a natural thing to type; accept it.
        let authority = if without_scheme.chars().all(|c| c.is_ascii_digit()) {
            format!("127.0.0.1:{without_scheme}")
        } else if without_scheme.contains(':') {
            without_scheme.to_string()
        } else {
            format!("{without_scheme}:7878")
        };
        Self {
            display_url: format!("http://{authority}"),
            authority,
        }
    }

    pub fn url(&self) -> &str {
        &self.display_url
    }

    /// GET `path` (including query string) and parse the body as JSON.
    ///
    /// `timeout` must exceed any server-side long-poll the request triggers,
    /// or the client will give up on a request the daemon is still serving.
    pub async fn get(&self, path: &str, timeout: Duration) -> Result<Value, ClientError> {
        match tokio::time::timeout(timeout, self.get_inner(path)).await {
            Ok(res) => res,
            Err(_) => Err(ClientError::Timeout { after: timeout }),
        }
    }

    async fn get_inner(&self, path: &str) -> Result<Value, ClientError> {
        let mut stream = TcpStream::connect(&self.authority)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused => ClientError::NotRunning {
                    url: self.display_url.clone(),
                },
                _ => ClientError::Io(e.to_string()),
            })?;

        // `Connection: close` makes the response self-delimiting: the server
        // hangs up when it is done, so we can read to EOF without parsing
        // Content-Length or chunked framing.
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: tailflow-agent/{}\r\n\
             Accept: application/json\r\n\
             Connection: close\r\n\r\n",
            self.authority,
            env!("CARGO_PKG_VERSION"),
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| ClientError::Io(e.to_string()))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| ClientError::Io(e.to_string()))?;

        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| ClientError::Parse("response had no header terminator".into()))?;

        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| ClientError::Parse("response had no status line".into()))?;

        if !(200..300).contains(&status) {
            // The daemon reports argument errors as {"error": "..."}; surface
            // that rather than a raw JSON blob.
            let message = serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| body.trim().chars().take(400).collect());
            return Err(ClientError::Http { status, message });
        }

        serde_json::from_str(body).map_err(|e| ClientError::Parse(e.to_string()))
    }
}

// ── Query string building ─────────────────────────────────────────────────────

/// Accumulates `?k=v` pairs, percent-encoding values.
///
/// Encoding is not optional here: a `grep` value is a regex, and regexes are
/// made of exactly the characters that break a URL (`&`, `+`, `?`, spaces).
#[derive(Default)]
pub struct QueryString {
    parts: Vec<String>,
}

impl QueryString {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, key: &str, value: impl AsRef<str>) -> &mut Self {
        self.parts
            .push(format!("{key}={}", percent_encode(value.as_ref())));
        self
    }

    pub fn push_opt(&mut self, key: &str, value: Option<impl AsRef<str>>) -> &mut Self {
        if let Some(v) = value {
            self.push(key, v);
        }
        self
    }

    pub fn push_num(&mut self, key: &str, value: Option<impl ToString>) -> &mut Self {
        if let Some(v) = value {
            self.push(key, v.to_string());
        }
        self
    }

    pub fn build(&self, path: &str) -> String {
        if self.parts.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", self.parts.join("&"))
        }
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_accepts_full_url() {
        let c = DaemonClient::new("http://127.0.0.1:9000");
        assert_eq!(c.authority, "127.0.0.1:9000");
        assert_eq!(c.url(), "http://127.0.0.1:9000");
    }

    #[test]
    fn resolve_accepts_bare_port() {
        assert_eq!(DaemonClient::new("9000").authority, "127.0.0.1:9000");
    }

    #[test]
    fn resolve_accepts_host_without_port() {
        assert_eq!(DaemonClient::new("localhost").authority, "localhost:7878");
    }

    #[test]
    fn resolve_strips_trailing_slash() {
        assert_eq!(
            DaemonClient::new("http://127.0.0.1:7878/").authority,
            "127.0.0.1:7878"
        );
    }

    #[test]
    fn percent_encode_escapes_regex_metacharacters() {
        assert_eq!(percent_encode("a&b"), "a%26b");
        assert_eq!(percent_encode("err(or)+"), "err%28or%29%2B");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("safe-_.~"), "safe-_.~");
    }

    #[test]
    fn query_string_omits_empty_params() {
        let mut qs = QueryString::new();
        qs.push_opt("source", None::<String>);
        assert_eq!(qs.build("/api/errors"), "/api/errors");
    }

    #[test]
    fn query_string_joins_params() {
        let mut qs = QueryString::new();
        qs.push("grep", "panic|fatal");
        qs.push_num("limit", Some(20));
        assert_eq!(
            qs.build("/api/query"),
            "/api/query?grep=panic%7Cfatal&limit=20"
        );
    }
}
