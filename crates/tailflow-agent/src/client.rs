//! Minimal HTTP client for the local TailFlow daemon.
//!
//! Deliberately hand-rolled rather than pulling in a general HTTP stack: every
//! request is a plain GET to `127.0.0.1` with no TLS and no redirects. A full
//! client would add a large dependency tree to two binaries whose entire job
//! is to shuttle JSON across a loopback socket.
//!
//! Authentication (daemon 0.4 / M4): every request carries
//! `Authorization: Bearer <token>`. The token is `$TAILFLOW_TOKEN` when set,
//! otherwise the one the daemon wrote to its owner-only runtime file
//! `<data dir>/run/daemon-<port>.json` (looked up only for loopback hosts).
//! Older daemons ignore the header.

use serde_json::Value;
use std::fmt;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_URL: &str = "http://127.0.0.1:7878";
/// Env var override, so an agent inherits the right port without a flag.
pub const URL_ENV: &str = "TAILFLOW_URL";
/// API token override (another OS user, a container, a remote port forward).
pub const TOKEN_ENV: &str = "TAILFLOW_TOKEN";

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
    /// The daemon answered, but demonstrably ignored an argument. Older daemons
    /// drop query parameters they do not recognise, which would turn a stricter
    /// request into a silently weaker one.
    Unsupported {
        detail: String,
    },
    /// The daemon refused the credential (401/403). Kept apart from `Http` so
    /// a caller never mistakes "not allowed to look" for "nothing there".
    Unauthorized {
        url: String,
        message: String,
        token: TokenSource,
    },
}

/// Where the client's token came from.
#[derive(Debug, Clone)]
pub enum TokenSource {
    Env,
    RuntimeFile(std::path::PathBuf),
    /// No token: nothing in the environment, and no readable runtime file
    /// (the path that was tried, if any).
    None(Option<std::path::PathBuf>),
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
            ClientError::Unsupported { detail } => write!(f, "{detail}"),
            ClientError::Unauthorized {
                url,
                message,
                token,
            } => {
                writeln!(f, "the daemon at {url} refused access: {message}")?;
                match token {
                    TokenSource::Env => write!(
                        f,
                        "The token came from ${TOKEN_ENV}; it changes every time the daemon \
                         starts unless the daemon was given the same ${TOKEN_ENV}."
                    ),
                    TokenSource::RuntimeFile(p) => write!(
                        f,
                        "The token came from {}. If that daemon is no longer the one \
                         listening, restart it or set ${TOKEN_ENV}.",
                        p.display()
                    ),
                    TokenSource::None(Some(p)) => write!(
                        f,
                        "No token found: {} does not exist or is unreadable. Run this \
                         client as the user that started the daemon (with the same \
                         TAILFLOW_DATA_DIR), or set ${TOKEN_ENV} \
                         (`tailflow-daemon token` prints it).",
                        p.display()
                    ),
                    TokenSource::None(None) => write!(
                        f,
                        "No token found. Set ${TOKEN_ENV} (`tailflow-daemon token` prints it)."
                    ),
                }
            }
        }
    }
}

impl std::error::Error for ClientError {}

pub struct DaemonClient {
    /// `host:port`, already stripped of scheme and trailing slash.
    authority: String,
    display_url: String,
    /// Runtime file of the daemon on this port (loopback hosts only).
    runtime_file: Option<std::path::PathBuf>,
    /// Current token and where it came from. Re-resolved after a refusal:
    /// a long-running MCP server outlives daemon restarts, and every restart
    /// rotates the token.
    credential: std::sync::Mutex<(Option<String>, TokenSource)>,
}

/// A token is sent in a header line, so it may not contain anything that
/// could end or extend that line.
fn header_safe(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 256
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

fn is_loopback_host(authority: &str) -> bool {
    let host = match authority.strip_prefix("[::1]") {
        Some(_) => "[::1]",
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// The runtime file of the daemon on `authority`, for loopback hosts.
fn runtime_file_for(authority: &str) -> Option<std::path::PathBuf> {
    if !is_loopback_host(authority) {
        return None;
    }
    let port = authority
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())?;
    crate::paths::runtime_file_for(port)
}

/// `$TAILFLOW_TOKEN`, else the token in `runtime_file`.
fn resolve_token(runtime_file: Option<&std::path::Path>) -> (Option<String>, TokenSource) {
    if let Some(t) = std::env::var(TOKEN_ENV).ok().filter(|v| !v.is_empty()) {
        return if header_safe(&t) {
            (Some(t), TokenSource::Env)
        } else {
            (None, TokenSource::None(None))
        };
    }
    let Some(path) = runtime_file else {
        return (None, TokenSource::None(None));
    };
    match read_runtime_token(path) {
        Some(t) => (Some(t), TokenSource::RuntimeFile(path.to_path_buf())),
        None => (None, TokenSource::None(Some(path.to_path_buf()))),
    }
}

fn read_runtime_token(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(64 * 1024).read_to_end(&mut buf).ok()?;
    let v: Value = serde_json::from_slice(&buf).ok()?;
    v.get("token")
        .and_then(Value::as_str)
        .filter(|t| header_safe(t))
        .map(str::to_string)
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
        // Connect to the IPv4 loopback for `localhost`: never let name
        // resolution pick an address another process might hold.
        let authority = match authority.strip_prefix("localhost:") {
            Some(port) => format!("127.0.0.1:{port}"),
            None => authority,
        };
        let runtime_file = runtime_file_for(&authority);
        Self::with_runtime_file(authority, runtime_file)
    }

    /// A client for `authority` that reads its token from `runtime_file`
    /// (unless `$TAILFLOW_TOKEN` is set).
    fn with_runtime_file(authority: String, runtime_file: Option<std::path::PathBuf>) -> Self {
        let credential = resolve_token(runtime_file.as_deref());
        Self {
            display_url: format!("http://{authority}"),
            authority,
            runtime_file,
            credential: std::sync::Mutex::new(credential),
        }
    }

    fn credential(&self) -> (Option<String>, TokenSource) {
        self.credential
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn url(&self) -> &str {
        &self.display_url
    }

    /// GET `path` (including query string) and parse the body as JSON.
    ///
    /// `timeout` must exceed any server-side long-poll the request triggers,
    /// or the client will give up on a request the daemon is still serving.
    pub async fn get(&self, path: &str, timeout: Duration) -> Result<Value, ClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let (token, _) = self.credential();
        let first =
            match tokio::time::timeout_at(deadline, self.get_inner(path, token.as_deref())).await {
                Ok(res) => res,
                Err(_) => return Err(ClientError::Timeout { after: timeout }),
            };
        // Refused: the daemon may have restarted with a new token. Re-read it
        // once; retry only if it actually changed (GET requests are safe to
        // repeat).
        let Err(ClientError::Unauthorized { .. }) = &first else {
            return first;
        };
        let fresh = resolve_token(self.runtime_file.as_deref());
        if fresh.0.is_none() || fresh.0 == token {
            return first;
        }
        let retry_token = fresh.0.clone();
        *self
            .credential
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
        match tokio::time::timeout_at(deadline, self.get_inner(path, retry_token.as_deref())).await
        {
            Ok(res) => res,
            Err(_) => Err(ClientError::Timeout { after: timeout }),
        }
    }

    async fn get_inner(&self, path: &str, token: Option<&str>) -> Result<Value, ClientError> {
        let mut stream = TcpStream::connect(&self.authority)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::PermissionDenied => {
                    ClientError::NotRunning {
                        url: self.display_url.clone(),
                    }
                }
                _ => ClientError::Io(e.to_string()),
            })?;

        // `Connection: close` makes the response self-delimiting: the server
        // hangs up when it is done, so we can read to EOF without parsing
        // Content-Length or chunked framing.
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: tailflow-agent/{}\r\n\
             Accept: application/json\r\n\
             {auth}\
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
            if status == 401 || status == 403 {
                return Err(ClientError::Unauthorized {
                    url: self.display_url.clone(),
                    message,
                    token: self.credential().1,
                });
            }
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
        assert_eq!(DaemonClient::new("localhost").authority, "127.0.0.1:7878");
    }

    #[test]
    fn resolve_strips_trailing_slash() {
        assert_eq!(
            DaemonClient::new("http://127.0.0.1:7878/").authority,
            "127.0.0.1:7878"
        );
    }

    /// A fake daemon that accepts only `good` and answers `n` requests.
    async fn fake_daemon(
        good: &'static str,
        n: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..n {
                let (mut sock, _) = listener.accept().await.unwrap();
                // Read the whole request head: it may arrive in several
                // segments, and closing with unread bytes resets the socket.
                let mut req = Vec::new();
                let mut chunk = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&chunk[..n]);
                }
                let req = String::from_utf8_lossy(&req).to_string();
                let ok = req.contains(&format!("Authorization: Bearer {good}\r\n"));
                seen.push(req);
                let resp = if ok {
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}"
                } else {
                    "HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":\"no\",\"kind\":\"invalid_token\"}"
                };
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
            seen
        });
        (addr, handle)
    }

    fn write_runtime(path: &std::path::Path, token: &str) {
        std::fs::write(path, format!("{{\"port\":1,\"token\":\"{token}\"}}")).unwrap();
    }

    #[tokio::test]
    async fn a_rotated_token_is_picked_up_after_a_refusal() {
        if std::env::var_os(TOKEN_ENV).is_some() {
            return; // the environment pins the token
        }
        let old = "a".repeat(64);
        let new = "b".repeat(64);
        let new: &'static str = Box::leak(new.into_boxed_str());
        let dir = tempfile_dir();
        let file = dir.join("daemon.json");
        write_runtime(&file, &old);
        let (addr, server) = fake_daemon(new, 3).await;
        let client = DaemonClient::with_runtime_file(addr, Some(file.clone()));
        // The daemon restarted: the file now holds the new token.
        write_runtime(&file, new);
        let v = client.get("/health", Duration::from_secs(5)).await.unwrap();
        assert_eq!(v["ok"], true);
        // The new token is kept for later calls.
        client.get("/health", Duration::from_secs(5)).await.unwrap();
        let seen = server.await.unwrap();
        assert!(seen[0].contains(&old), "first try used the old token");
        assert!(seen[1].contains(new) && seen[2].contains(new));
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_refusal_without_a_new_token_is_reported_not_retried() {
        if std::env::var_os(TOKEN_ENV).is_some() {
            return;
        }
        let dir = tempfile_dir();
        let file = dir.join("daemon.json");
        write_runtime(&file, &"c".repeat(64));
        let (addr, server) = fake_daemon("never", 1).await;
        let client = DaemonClient::with_runtime_file(addr, Some(file));
        let err = client
            .get("/health", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Unauthorized { .. }), "{err}");
        assert!(err.to_string().contains("refused access"), "{err}");
        assert_eq!(server.await.unwrap().len(), 1, "no blind retry");
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    /// A directory unique to this test (a counter, not a timestamp:
    /// concurrent tests can start within the clock's resolution).
    fn tempfile_dir() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("tf-agent-test-{}-{n}", std::process::id()));
        let _stale = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_loopback_hosts_get_the_runtime_file_token() {
        assert!(is_loopback_host("127.0.0.1:7878"));
        assert!(is_loopback_host("localhost:7878"));
        assert!(is_loopback_host("[::1]:7878"));
        assert!(!is_loopback_host("192.168.1.5:7878"));
        assert!(!is_loopback_host("127.0.0.1.nip.io:7878"));
    }

    #[test]
    fn tokens_that_could_inject_headers_are_refused() {
        assert!(header_safe(&"a".repeat(64)));
        assert!(!header_safe("abc\r\nX-Evil: 1"));
        assert!(!header_safe("abc def"));
        assert!(!header_safe(""));
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
