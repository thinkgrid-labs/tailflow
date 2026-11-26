//! Local API access control (M4-A).
//!
//! Loopback is not a security boundary on its own: every OS user on the
//! machine, every container sharing the host network, and every web page in
//! the browser can open a connection to `127.0.0.1`. The API exposes agent
//! sessions, project paths, file paths, commands and runtime logs, so being
//! read-only does not make it harmless.
//!
//! Credentials:
//!
//! - **API token** (CLI, MCP, scripts): 256 random bits generated on every
//!   start. It is written to an owner-only *runtime file*,
//!   `<data dir>/run/daemon-<port>.json`, which local clients read. Sent as
//!   `Authorization: Bearer <token>`. `$TAILFLOW_TOKEN` overrides it, for
//!   clients that cannot read the runtime file (another user, a container).
//! - **Browser session**: an `HttpOnly; SameSite=Strict` cookie holding 256
//!   random bits. It is created only by redeeming a **one-time sign-in code**
//!   at `GET /auth/login/<code>`. A code is single use, expires within
//!   minutes, and is minted only by a holder of the API token (the startup
//!   banner, or `tailflow-daemon dashboard`). No endpoint ever returns the
//!   token or a session to a caller that does not already hold a credential.
//!   Sessions survive a daemon restart on the same port (an owner-only file
//!   next to the runtime file), so an open dashboard reconnects instead of
//!   asking to sign in again; they end after [`SESSION_IDLE`] unused, after
//!   [`SESSION_MAX_AGE`], on sign-out, or on `dashboard --revoke-sessions`.
//!
//! Browser cookies are **not** isolated by port. A cookie set by
//! `127.0.0.1:7878` is also sent to `127.0.0.1:3000`, and pages on other
//! localhost ports can set cookies this daemon then receives. So:
//!
//! - the cookie name carries the port, so two daemons do not overwrite each
//!   other's sessions;
//! - a cookie is accepted only when the browser's Fetch Metadata says the
//!   request came from this origin (`Sec-Fetch-Site: same-origin`) or from
//!   the user directly (`none`: typed URL, bookmark, a link opened from a
//!   terminal). A page on another localhost port is `same-site`, so its
//!   requests are refused even though the browser attaches the cookie;
//! - an `Origin` header, when present, must name exactly this origin;
//! - several cookies with the session name are tolerated (cookie tossing
//!   from another port cannot shadow the real one).
//!
//! What this does not protect against: a process running as the same OS
//! user can read the runtime file and the database itself, so it is outside
//! the threat model. See `docs/security.md`.

use axum::http::{header, HeaderMap};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Environment variable carrying an API token. Read by the daemon (use this
/// token instead of a random one) and by every client.
pub const TOKEN_ENV: &str = "TAILFLOW_TOKEN";

/// Version of the runtime file format.
pub const RUNTIME_FILE_VERSION: u32 = 1;

/// Browser sessions kept at once. The oldest is evicted beyond this.
const MAX_SESSIONS: usize = 64;
/// A session unused for this long expires.
pub const SESSION_IDLE: Duration = Duration::from_secs(12 * 3600);
/// No session lives longer than this, however often it is used.
pub const SESSION_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Persist `last_used` at most this often (it is also written on shutdown).
const TOUCH_PERSIST_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Outstanding sign-in codes kept at once. The oldest is dropped beyond this.
const MAX_CODES: usize = 16;
/// Lifetime of the code printed in the startup banner.
pub const STARTUP_CODE_TTL: Duration = Duration::from_secs(60 * 60);
/// Lifetime of a code minted with `tailflow-daemon dashboard`, which opens
/// the browser right away.
///
/// Short: the link is opened right away, and on Linux another user may read
/// process arguments (the browser opener is given a private file, not the
/// link, but a printed link can still be copied around).
pub const MINTED_CODE_TTL: Duration = Duration::from_secs(60);
/// Persisted sessions are restored only if the store was written this
/// recently (a restart, not a daemon that has been gone for a while). Bounds
/// how long a cookie captured by something listening on the port while the
/// daemon was down stays useful (see docs/security.md).
pub const RESTORE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Accepted `$TAILFLOW_TOKEN` length. 32 characters is the minimum that
/// still carries a reasonable amount of entropy.
const MIN_TOKEN_LEN: usize = 32;
const MAX_TOKEN_LEN: usize = 256;

/// 32 random bytes from the OS CSPRNG, hex encoded.
///
/// # Errors
/// The OS random source is unavailable. Credentials are never derived from
/// anything weaker.
pub fn random_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("OS random source unavailable: {e}"))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

/// Constant-time equality, so a comparison's duration does not reveal how
/// much of a guessed credential was right. Length is not secret (fixed).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Validate a token given through `$TAILFLOW_TOKEN`.
///
/// # Errors
/// Wrong length or characters outside the URL-safe set.
pub fn validate_token(token: &str) -> Result<(), String> {
    let len = token.len();
    if !(MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&len) {
        return Err(format!(
            "${TOKEN_ENV} must be {MIN_TOKEN_LEN}..={MAX_TOKEN_LEN} characters (got {len})"
        ));
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    {
        return Err(format!(
            "${TOKEN_ENV} may contain only letters, digits and - _ . ~"
        ));
    }
    Ok(())
}

/// A secret that never prints itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Which credential authorized a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    /// `Authorization: Bearer <api token>`.
    Token,
    /// The browser session cookie.
    Session,
}

/// Why a request was not authorized. Every protected path answers these the
/// same way, whether or not the path exists, so a refused caller learns
/// nothing about the data behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// No credential at all.
    Missing,
    /// A bearer token that is not this run's token.
    BadToken,
    /// A session cookie that is unknown or expired (for example after a
    /// sign-out, a revocation, idle or age expiry, or a restart more than
    /// `RESTORE_WINDOW` after the session store was last written).
    SessionExpired,
    /// A valid session cookie on a request the browser says did not come
    /// from this origin (another localhost port, another site) or without
    /// Fetch Metadata at all.
    CrossOrigin,
}

impl Denied {
    pub fn kind(self) -> &'static str {
        match self {
            Denied::Missing => "unauthorized",
            Denied::BadToken => "invalid_token",
            Denied::SessionExpired => "session_expired",
            Denied::CrossOrigin => "cross_origin",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Denied::Missing => {
                "TailFlow requires a credential. Open the dashboard with \
                 `tailflow-daemon dashboard`; CLI and MCP clients read the token from \
                 the daemon's runtime file or $TAILFLOW_TOKEN"
            }
            Denied::BadToken => {
                "the API token is not valid for this daemon run (the token changes on \
                 every start; re-read the runtime file or update $TAILFLOW_TOKEN)"
            }
            Denied::SessionExpired => {
                "this browser session is no longer valid (it expired, was signed out or \
                 revoked). Run `tailflow-daemon dashboard` for a new sign-in link"
            }
            Denied::CrossOrigin => {
                "browser session credentials are accepted only from the TailFlow \
                 dashboard's own origin"
            }
        }
    }
}

/// One browser session. Times are Unix seconds (wall clock), so they mean
/// the same thing after a restart.
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    secret: String,
    created: i64,
    last_used: i64,
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

impl Session {
    fn live(&self, now: i64) -> bool {
        now.saturating_sub(self.last_used) < secs(SESSION_IDLE)
            && now.saturating_sub(self.created) < secs(SESSION_MAX_AGE)
    }
}

/// On-disk form of the session store.
#[derive(Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    port: u16,
    /// Unix seconds of the write (older stores are not restored).
    #[serde(default)]
    saved_at: i64,
    sessions: Vec<Session>,
}

struct LoginCode {
    secret: String,
    expires: Instant,
}

/// Counters exposed in authenticated diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct AuthView {
    pub sessions_active: usize,
    pub sign_in_codes_outstanding: usize,
    pub refused_requests: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refused_at: Option<chrono::DateTime<chrono::Utc>>,
    pub token_source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_file: Option<String>,
}

/// Access control state for one daemon run.
pub struct Auth {
    token: Secret,
    /// The token came from `$TAILFLOW_TOKEN` rather than this run.
    fixed_token: bool,
    port: OnceLock<u16>,
    runtime_file: OnceLock<PathBuf>,
    /// Where sessions persist across restarts (owner-only). Unset: memory.
    session_file: OnceLock<PathBuf>,
    /// Held across a snapshot and its write, so writes land in order and an
    /// older snapshot (with a since-revoked session) never overwrites a
    /// newer one.
    persist_lock: Mutex<()>,
    sessions: Mutex<Vec<Session>>,
    /// Unix seconds of the last persisted `last_used` refresh.
    last_touch_persist: AtomicI64,
    codes: Mutex<Vec<LoginCode>>,
    refused: AtomicU64,
    last_refused_us: AtomicI64,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("token", &self.token)
            .field("port", &self.port.get())
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// Use `$TAILFLOW_TOKEN` when set, else a fresh random token.
    ///
    /// # Errors
    /// An invalid `$TAILFLOW_TOKEN`, or no OS random source.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(TOKEN_ENV).ok().filter(|v| !v.is_empty()) {
            Some(t) => {
                validate_token(&t)?;
                Ok(Self::with_token(t, true))
            }
            None => Ok(Self::with_token(random_secret()?, false)),
        }
    }

    /// A random token (tests and memory-only constructors).
    pub fn generate() -> Self {
        // The OS random source failing is not survivable for a credential;
        // tests and the in-memory constructors have no error path.
        let token = random_secret().expect("the OS random source is available");
        Self::with_token(token, false)
    }

    fn with_token(token: String, fixed: bool) -> Self {
        Self {
            token: Secret(token),
            fixed_token: fixed,
            port: OnceLock::new(),
            runtime_file: OnceLock::new(),
            session_file: OnceLock::new(),
            persist_lock: Mutex::new(()),
            sessions: Mutex::new(Vec::new()),
            last_touch_persist: AtomicI64::new(0),
            codes: Mutex::new(Vec::new()),
            refused: AtomicU64::new(0),
            last_refused_us: AtomicI64::new(0),
        }
    }

    pub fn token(&self) -> &Secret {
        &self.token
    }

    /// Record the port actually bound (it names the session cookie).
    pub fn set_port(&self, port: u16) {
        let _already_set = self.port.set(port);
    }

    /// Session cookie name. Includes the port because browsers send a
    /// loopback cookie to every port of the same host.
    pub fn cookie_name(&self) -> String {
        format!("tailflow_session_{}", self.port.get().copied().unwrap_or(0))
    }

    pub fn set_runtime_file(&self, path: PathBuf) {
        let _already_set = self.runtime_file.set(path);
    }

    pub fn note_refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        self.last_refused_us
            .store(chrono::Utc::now().timestamp_micros(), Ordering::Relaxed);
    }

    /// Keep sessions in `path` from now on, loading the live ones a
    /// previous run on this port left there. A missing, unreadable or
    /// foreign file just means no sessions (never fatal).
    pub fn attach_session_store(&self, path: PathBuf) {
        let port = self.port.get().copied().unwrap_or(0);
        let now = now_secs();
        let loaded: Vec<Session> = read_bounded(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionFile>(&b).ok())
            .filter(|f| f.version == RUNTIME_FILE_VERSION && f.port == port)
            .filter(|f| now.saturating_sub(f.saved_at) <= secs(RESTORE_WINDOW))
            .map(|f| {
                f.sessions
                    .into_iter()
                    .filter(|s| s.live(now) && s.secret.len() == 64)
                    .take(MAX_SESSIONS)
                    .collect()
            })
            .unwrap_or_default();
        lock(&self.sessions).extend(loaded);
        let _already_set = self.session_file.set(path);
        // Rewrites the store at once, dropping anything stale left on disk.
        self.persist();
    }

    /// Write live sessions to the session store, if one is attached.
    ///
    /// Not flushed to stable storage: used only for writes that cannot
    /// make a revoked session valid again (startup, the idle-clock touch,
    /// shutdown). On filesystems that write file data before the metadata
    /// pointing to it (APFS, ext4 `data=ordered`, XFS, btrfs), losing it in
    /// an OS crash only loses sessions. Writes that remove sessions or sign
    /// one in use [`Auth::persist_durable`].
    pub fn persist(&self) {
        if let Err(e) = self.persist_with(Durability::Unsynced) {
            tracing::warn!(err = %e, "could not persist browser sessions");
        }
    }

    /// Like [`Auth::persist`], but flushed to stable storage (file, then
    /// the directory entry) before it returns, so a revoked, signed-out or
    /// evicted session cannot come back from an older store after a power
    /// loss, and a store whose blocks were freed cannot be reused under a
    /// later unflushed write. Used for sign-in, sign-out and revocation,
    /// which are rare.
    ///
    /// Fails closed: if the new store cannot be written, the old one (which
    /// may still hold the removed sessions) is deleted, so nothing is
    /// restored after a restart.
    fn persist_durable(&self) {
        let Err(e) = self.persist_with(Durability::Synced) else {
            return;
        };
        let Some(path) = self.session_file.get() else {
            return;
        };
        match std::fs::remove_file(path) {
            Ok(()) => {
                tracing::warn!(err = %e, "could not write the session store; removed it (browsers sign in again after a restart)");
                if let Some(dir) = path.parent() {
                    sync_dir(dir);
                }
            }
            Err(r) if r.kind() == std::io::ErrorKind::NotFound => {}
            Err(r) => tracing::error!(
                err = %e, remove_err = %r, path = %path.display(),
                "could not write or remove the session store: revoked sessions may be restored after a restart within 15 minutes"
            ),
        }
    }

    fn persist_with(&self, durability: Durability) -> std::io::Result<()> {
        let Some(path) = self.session_file.get() else {
            return Ok(());
        };
        let _ordered = lock(&self.persist_lock);
        let now = now_secs();
        let sessions: Vec<Session> = {
            let mut guard = lock(&self.sessions);
            guard.retain(|s| s.live(now));
            guard.clone()
        };
        let file = SessionFile {
            version: RUNTIME_FILE_VERSION,
            port: self.port.get().copied().unwrap_or(0),
            saved_at: now,
            sessions,
        };
        let result = serde_json::to_vec(&file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            .and_then(|body| write_private(path, &body, durability));
        self.last_touch_persist.store(now, Ordering::Relaxed);
        result
    }

    /// End every browser session (all browsers must sign in again).
    pub fn revoke_all_sessions(&self) -> usize {
        let n = {
            let mut s = lock(&self.sessions);
            let n = s.len();
            s.clear();
            n
        };
        self.persist_durable();
        n
    }

    pub fn view(&self) -> AuthView {
        let now = now_secs();
        let sessions = lock(&self.sessions).iter().filter(|s| s.live(now)).count();
        let instant_now = Instant::now();
        let codes = lock(&self.codes)
            .iter()
            .filter(|c| c.expires > instant_now)
            .count();
        let last = self.last_refused_us.load(Ordering::Relaxed);
        AuthView {
            sessions_active: sessions,
            sign_in_codes_outstanding: codes,
            refused_requests: self.refused.load(Ordering::Relaxed),
            last_refused_at: (last > 0)
                .then(|| chrono::DateTime::from_timestamp_micros(last))
                .flatten(),
            token_source: if self.fixed_token {
                "environment"
            } else {
                "generated_per_run"
            },
            runtime_file: self.runtime_file.get().map(|p| p.display().to_string()),
        }
    }

    /// Mint a one-time sign-in code valid for `ttl`.
    ///
    /// # Errors
    /// No OS random source.
    pub fn mint_code(&self, ttl: Duration) -> Result<(String, Instant), String> {
        let secret = random_secret()?;
        let expires = Instant::now() + ttl;
        let mut codes = lock(&self.codes);
        let now = Instant::now();
        codes.retain(|c| c.expires > now);
        while codes.len() >= MAX_CODES {
            codes.remove(0);
        }
        codes.push(LoginCode {
            secret: secret.clone(),
            expires,
        });
        Ok((secret, expires))
    }

    /// Redeem a sign-in code: on success it is consumed and a new session
    /// secret is returned. A code works once.
    ///
    /// # Errors
    /// `Err(())` for an unknown, used or expired code.
    pub fn redeem_code(&self, code: &str) -> Result<String, ()> {
        let now = Instant::now();
        {
            let mut codes = lock(&self.codes);
            codes.retain(|c| c.expires > now);
            // Compare against every code (constant work per code).
            let mut found = None;
            for (i, c) in codes.iter().enumerate() {
                if ct_eq(c.secret.as_bytes(), code.as_bytes()) {
                    found = Some(i);
                }
            }
            match found {
                Some(i) => {
                    codes.remove(i);
                }
                None => return Err(()),
            }
        }
        let secret = random_secret().map_err(|_| ())?;
        {
            let now = now_secs();
            let mut sessions = lock(&self.sessions);
            sessions.retain(|s| s.live(now));
            if sessions.len() >= MAX_SESSIONS {
                // Evict the least recently used.
                if let Some((i, _)) = sessions.iter().enumerate().min_by_key(|(_, s)| s.last_used) {
                    sessions.remove(i);
                }
            }
            sessions.push(Session {
                secret: secret.clone(),
                created: now,
                last_used: now,
            });
        }
        // Durable: a sign-in may evict a session, and its write must not
        // land in blocks freed by an earlier sign-out (see persist_durable).
        self.persist_durable();
        Ok(secret)
    }

    /// End the session carried by `value`, if it is one of ours.
    pub fn end_session(&self, value: &str) -> bool {
        let ended = {
            let mut sessions = lock(&self.sessions);
            let before = sessions.len();
            sessions.retain(|s| !ct_eq(s.secret.as_bytes(), value.as_bytes()));
            sessions.len() != before
        };
        if ended {
            self.persist_durable();
        }
        ended
    }

    fn check_session(&self, value: &str) -> bool {
        let now = now_secs();
        let valid = {
            let mut sessions = lock(&self.sessions);
            let mut hit = None;
            for (i, s) in sessions.iter().enumerate() {
                if ct_eq(s.secret.as_bytes(), value.as_bytes()) {
                    hit = Some(i);
                }
            }
            match hit.and_then(|i| sessions.get_mut(i)) {
                Some(s) if s.live(now) => {
                    s.last_used = now;
                    true
                }
                _ => false,
            }
        };
        // Keep the idle clock roughly right across restarts without a write
        // per request.
        if valid
            && now.saturating_sub(self.last_touch_persist.load(Ordering::Relaxed))
                >= secs(TOUCH_PERSIST_INTERVAL)
        {
            self.persist();
        }
        valid
    }

    /// Session cookie values sent with a request (there may be several:
    /// another localhost port can set cookies with the same name).
    pub fn session_cookies(&self, headers: &HeaderMap) -> Vec<String> {
        let name = self.cookie_name();
        let mut out = Vec::new();
        for value in headers.get_all(header::COOKIE) {
            let Ok(s) = value.to_str() else { continue };
            for pair in s.split(';') {
                if let Some((k, v)) = pair.trim().split_once('=') {
                    if k == name && !v.is_empty() && out.len() < 8 {
                        out.push(v.to_string());
                    }
                }
            }
        }
        out
    }

    /// Decide whether a request carries a valid credential.
    ///
    /// A bearer token, when present, is decisive: a wrong token is refused
    /// even if a valid cookie is attached too. A cookie counts only in a
    /// same-origin browser context (see the module docs).
    pub fn authorize(&self, headers: &HeaderMap) -> Result<Credential, Denied> {
        if let Some(value) = headers.get(header::AUTHORIZATION) {
            let presented = value
                .to_str()
                .ok()
                .and_then(|v| {
                    let (scheme, rest) = v.split_once(' ')?;
                    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
                })
                .unwrap_or("");
            return if ct_eq(presented.as_bytes(), self.token.expose().as_bytes()) {
                Ok(Credential::Token)
            } else {
                Err(Denied::BadToken)
            };
        }
        let cookies = self.session_cookies(headers);
        if cookies.is_empty() {
            return Err(Denied::Missing);
        }
        // Check every candidate (no early exit on the first match).
        let mut valid = false;
        for c in &cookies {
            valid |= self.check_session(c);
        }
        if !valid {
            return Err(Denied::SessionExpired);
        }
        if !same_origin_browser_context(headers) {
            return Err(Denied::CrossOrigin);
        }
        Ok(Credential::Session)
    }
}

/// The request was made by this origin's own pages, or by the user directly.
///
/// `Sec-Fetch-Site` is set by the browser and cannot be changed by page
/// script. `same-site` (another port on the same host name) and
/// `cross-site` are refused. A browser that sends no Fetch Metadata at all
/// is refused too: without it a page on another localhost port could ride
/// the cookie with `<img>`/`<script>` loads, which carry no `Origin`.
pub fn same_origin_browser_context(headers: &HeaderMap) -> bool {
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    if !matches!(site, Some("same-origin" | "none")) {
        return false;
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => {
            let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
            match (origin.to_str().ok(), host) {
                (Some(o), Some(h)) => o == format!("http://{h}"),
                _ => false,
            }
        }
    }
}

/// `Set-Cookie` value for a new session.
pub fn session_cookie(name: &str, value: &str) -> String {
    // No `Max-Age`: a browser-session cookie, and the server expires idle
    // sessions anyway. No `Secure`: the daemon serves plain HTTP on
    // loopback, and some browsers drop Secure cookies from http://127.0.0.1.
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict")
}

/// `Set-Cookie` value that removes the session cookie.
pub fn clear_cookie(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The guarded vectors are always left consistent (push/retain/remove
    // complete before any code that could panic), so a poisoned lock still
    // holds valid data.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ── Runtime file ─────────────────────────────────────────────────────────────

/// Contents of `<data dir>/run/daemon-<port>.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub version: u32,
    pub port: u16,
    pub pid: u32,
    pub instance_id: String,
    pub url: String,
    pub token: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// `<data dir>/run`.
pub fn runtime_dir() -> Option<PathBuf> {
    tailflow_core::paths::default_data_dir().map(|d| d.join("run"))
}

/// `<data dir>/run/daemon-<port>.json`.
pub fn runtime_file_for(port: u16) -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(format!("daemon-{port}.json")))
}

/// `<data dir>/run/sessions-<port>.json`: browser sessions of the daemon on
/// that port.
pub fn session_file_for(port: u16) -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(format!("sessions-{port}.json")))
}

/// Write the runtime file atomically and readable only by its owner.
///
/// # Errors
/// The directory or file cannot be created.
pub fn write_runtime_file(path: &Path, info: &RuntimeInfo) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(info)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Not flushed to stable storage: the file is rewritten at every start, a
    // process crash keeps it (page cache), and an OS crash also ends the
    // daemon whose token it holds. A torn file is refused as malformed by
    // clients (fails closed).
    write_private(path, &body, Durability::Unsynced)
}

/// Whether [`write_private`] flushes the file to stable storage before the
/// rename. On macOS a flush is `F_FULLFSYNC` (about 20 ms on an internal
/// SSD), which M4 paid twice at startup and once at shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Durability {
    Synced,
    Unsynced,
}

/// Atomically replace `path` with `body`, owner-only (`0600` in a `0700`
/// directory on Unix). Readers see the old file or the new one, never a
/// partial write. On Unix, `Synced` also survives a power loss (file and
/// directory are flushed); on Windows the rename itself is not flushed.
fn write_private(path: &Path, body: &[u8], durability: Durability) -> std::io::Result<()> {
    use std::sync::atomic::AtomicU64 as Counter;
    static NEXT: Counter = Counter::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    tailflow_core::paths::create_private_dir(dir)?;
    // A unique name created exclusively: an existing file or symlink planted
    // at the temporary path makes the write fail instead of following it.
    // The random part keeps a file left by a crash from colliding with a
    // later run that reuses the pid and counter (after a reboot).
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.{}.tmp",
        path.file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy()),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        u64::from_le_bytes(nonce)
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    #[cfg(unix)]
    {
        // The directory must be ours and private: a pre-created run
        // directory (a shared TAILFLOW_DATA_DIR) could otherwise expose or
        // redirect the credential.
        use std::os::unix::fs::MetadataExt;
        let mine = f.metadata()?.uid();
        let d = std::fs::metadata(dir)?;
        if d.uid() != mine || d.mode() & 0o077 != 0 {
            drop(f);
            let _cleanup = std::fs::remove_file(&tmp);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a private directory owned by this user; not writing credentials there",
                    dir.display()
                ),
            ));
        }
    }
    let written = std::io::Write::write_all(&mut f, body).and_then(|()| {
        if durability == Durability::Synced {
            f.sync_all()
        } else {
            Ok(())
        }
    });
    drop(f);
    if let Err(e) = written {
        let _cleanup = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _cleanup = std::fs::remove_file(&tmp);
    })?;
    // A synced write must also make the rename durable, or a power loss
    // could bring back the previous file (for a revocation: the revoked
    // session).
    if durability == Durability::Synced {
        sync_dir(dir);
    }
    Ok(())
}

/// Flush a directory's entries (Unix). Opened with `O_DIRECTORY` and
/// `O_NOFOLLOW`, so a symlink or a FIFO planted at the path fails at once
/// instead of being followed or blocking. Windows: no-op (a directory
/// cannot be flushed through std).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(dir);
        if let Err(e) = opened.and_then(|d| d.sync_all()) {
            tracing::warn!(err = %e, dir = %dir.display(), "could not flush the directory");
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Read a runtime file, bounded in size.
///
/// # Errors
/// Missing, unreadable, oversized or malformed.
pub fn read_runtime_file(path: &Path) -> Result<RuntimeInfo, String> {
    let buf = read_bounded(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&buf).map_err(|e| format!("{}: malformed ({e})", path.display()))
}

/// Read at most 64 KiB (runtime files) — a 64-session store is ~10 KiB.
fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(64 * 1024).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Remove the runtime file if it still belongs to `instance_id` (another
/// daemon may have bound the port and rewritten it since).
pub fn remove_runtime_file(path: &Path, instance_id: &str) {
    if read_runtime_file(path).is_ok_and(|i| i.instance_id == instance_id) {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::debug!(err = %e, "could not remove the runtime file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn auth() -> Auth {
        let a = Auth::generate();
        a.set_port(7878);
        a
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn session(a: &Auth) -> String {
        let (code, _) = a.mint_code(MINTED_CODE_TTL).unwrap();
        a.redeem_code(&code).unwrap()
    }

    #[test]
    fn secrets_are_256_bit_hex_and_distinct() {
        let a = random_secret().unwrap();
        let b = random_secret().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn debug_output_never_contains_the_token() {
        let a = auth();
        let token = a.token().expose().to_string();
        assert!(!format!("{a:?}").contains(&token));
        assert!(!format!("{:?}", a.token()).contains(&token));
    }

    #[test]
    fn ct_eq_matches_only_identical_input() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"a"));
    }

    #[test]
    fn env_token_is_validated() {
        assert!(validate_token(&"a".repeat(31)).is_err());
        assert!(validate_token(&"a".repeat(32)).is_ok());
        assert!(validate_token(&"a".repeat(257)).is_err());
        assert!(validate_token(&format!("{}\n", "a".repeat(40))).is_err());
        assert!(validate_token(&format!("{} x", "a".repeat(40))).is_err());
    }

    #[test]
    fn bearer_token_is_decisive() {
        let a = auth();
        let ok = format!("Bearer {}", a.token().expose());
        assert_eq!(
            a.authorize(&headers(&[("authorization", &ok)])),
            Ok(Credential::Token)
        );
        let lower = format!("bearer {}", a.token().expose());
        assert_eq!(
            a.authorize(&headers(&[("authorization", &lower)])),
            Ok(Credential::Token)
        );
        assert_eq!(
            a.authorize(&headers(&[("authorization", "Bearer nope")])),
            Err(Denied::BadToken)
        );
        assert_eq!(
            a.authorize(&headers(&[("authorization", "Basic abc")])),
            Err(Denied::BadToken)
        );
        // A wrong token is refused even with a valid session cookie.
        let s = session(&a);
        let cookie = format!("{}={s}", a.cookie_name());
        assert_eq!(
            a.authorize(&headers(&[
                ("authorization", "Bearer wrong"),
                ("cookie", &cookie),
                ("sec-fetch-site", "same-origin"),
            ])),
            Err(Denied::BadToken)
        );
    }

    #[test]
    fn missing_credentials_are_refused() {
        let a = auth();
        assert_eq!(a.authorize(&HeaderMap::new()), Err(Denied::Missing));
        // A cookie for another port is not ours.
        let s = session(&a);
        assert_eq!(
            a.authorize(&headers(&[
                ("cookie", &format!("tailflow_session_7879={s}")),
                ("sec-fetch-site", "same-origin")
            ])),
            Err(Denied::Missing)
        );
    }

    #[test]
    fn session_cookie_needs_a_same_origin_browser_context() {
        let a = auth();
        let s = session(&a);
        let cookie = format!("{}={s}", a.cookie_name());
        let ok = |site: Option<&str>, origin: Option<&str>| {
            let mut h = headers(&[("cookie", &cookie), ("host", "127.0.0.1:7878")]);
            if let Some(site) = site {
                h.insert("sec-fetch-site", HeaderValue::from_str(site).unwrap());
            }
            if let Some(o) = origin {
                h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
            }
            a.authorize(&h)
        };
        assert_eq!(ok(Some("same-origin"), None), Ok(Credential::Session));
        assert_eq!(
            ok(Some("same-origin"), Some("http://127.0.0.1:7878")),
            Ok(Credential::Session)
        );
        assert_eq!(ok(Some("none"), None), Ok(Credential::Session));
        // Another localhost port: same-site to the browser.
        assert_eq!(ok(Some("same-site"), None), Err(Denied::CrossOrigin));
        assert_eq!(
            ok(Some("same-site"), Some("http://127.0.0.1:3000")),
            Err(Denied::CrossOrigin)
        );
        assert_eq!(ok(Some("cross-site"), None), Err(Denied::CrossOrigin));
        // No Fetch Metadata (old browser, or a replayed cookie).
        assert_eq!(ok(None, None), Err(Denied::CrossOrigin));
        // Origin of another port even though the site claims same-origin.
        assert_eq!(
            ok(Some("same-origin"), Some("http://127.0.0.1:3000")),
            Err(Denied::CrossOrigin)
        );
    }

    #[test]
    fn tossed_cookies_cannot_shadow_the_real_session() {
        let a = auth();
        let s = session(&a);
        let name = a.cookie_name();
        let h = headers(&[
            ("cookie", &format!("{name}=attacker; {name}={s}")),
            ("cookie", &format!("{name}=junk")),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert_eq!(a.authorize(&h), Ok(Credential::Session));
        let h = headers(&[
            ("cookie", &format!("{name}=attacker")),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert_eq!(a.authorize(&h), Err(Denied::SessionExpired));
    }

    #[test]
    fn codes_are_single_use_and_expire() {
        let a = auth();
        let (code, _) = a.mint_code(MINTED_CODE_TTL).unwrap();
        assert!(a.redeem_code(&code).is_ok());
        assert!(a.redeem_code(&code).is_err(), "a code works once");
        assert!(a.redeem_code("").is_err());
        assert!(a.redeem_code(&"0".repeat(64)).is_err());
        let (short, _) = a.mint_code(Duration::from_millis(1)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(a.redeem_code(&short).is_err(), "expired");
    }

    #[test]
    fn outstanding_codes_and_sessions_are_bounded() {
        let a = auth();
        let first = a.mint_code(MINTED_CODE_TTL).unwrap().0;
        for _ in 0..MAX_CODES {
            let _code = a.mint_code(MINTED_CODE_TTL).unwrap();
        }
        assert!(a.redeem_code(&first).is_err(), "oldest code dropped");
        assert!(a.view().sign_in_codes_outstanding <= MAX_CODES);
        for _ in 0..(MAX_SESSIONS + 10) {
            let _session = session(&a);
        }
        assert_eq!(a.view().sessions_active, MAX_SESSIONS);
    }

    #[test]
    fn a_new_run_rejects_the_previous_runs_credentials() {
        let old = auth();
        let s = session(&old);
        let token = old.token().expose().to_string();
        let new = auth();
        let cookie = format!("{}={s}", new.cookie_name());
        assert_eq!(
            new.authorize(&headers(&[
                ("cookie", &cookie),
                ("sec-fetch-site", "same-origin")
            ])),
            Err(Denied::SessionExpired)
        );
        assert_eq!(
            new.authorize(&headers(&[("authorization", &format!("Bearer {token}"))])),
            Err(Denied::BadToken)
        );
    }

    #[test]
    fn ended_sessions_stop_working() {
        let a = auth();
        let s = session(&a);
        assert!(a.end_session(&s));
        let cookie = format!("{}={s}", a.cookie_name());
        assert_eq!(
            a.authorize(&headers(&[
                ("cookie", &cookie),
                ("sec-fetch-site", "same-origin")
            ])),
            Err(Denied::SessionExpired)
        );
    }

    #[test]
    fn sessions_survive_a_restart_on_the_same_port_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/sessions-7878.json");
        let first = auth();
        first.attach_session_store(path.clone());
        let s = session(&first);
        let ended = session(&first);
        assert!(first.end_session(&ended));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains(&s) && !on_disk.contains(&ended));

        // Restart: new token, same sessions.
        let second = auth();
        second.attach_session_store(path.clone());
        let ok = |a: &Auth, v: &str| {
            a.authorize(&headers(&[
                ("cookie", &format!("{}={v}", a.cookie_name())),
                ("sec-fetch-site", "same-origin"),
            ]))
        };
        assert_eq!(ok(&second, &s), Ok(Credential::Session));
        assert_eq!(ok(&second, &ended), Err(Denied::SessionExpired));
        assert_ne!(second.token().expose(), first.token().expose());

        // Another port does not load them.
        let other = Auth::generate();
        other.set_port(7879);
        other.attach_session_store(path.clone());
        assert_eq!(other.view().sessions_active, 0);

        // Revocation ends them everywhere, on disk too.
        assert_eq!(second.revoke_all_sessions(), 1);
        assert_eq!(ok(&second, &s), Err(Denied::SessionExpired));
        let third = auth();
        third.attach_session_store(path);
        assert_eq!(ok(&third, &s), Err(Denied::SessionExpired));
    }

    /// M4-F: a revocation that cannot be written removes the old store
    /// (which still holds the revoked session) instead of leaving it to be
    /// restored after a restart.
    #[cfg(unix)]
    #[test]
    fn a_revocation_that_cannot_be_written_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let path = run.join("sessions-7878.json");
        let first = auth();
        first.attach_session_store(path.clone());
        let s = session(&first);
        assert!(std::fs::read_to_string(&path).unwrap().contains(&s));
        // The directory stops being private: write_private refuses it.
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(first.revoke_all_sessions(), 1);
        assert!(!path.exists(), "the stale store must be gone");
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let second = auth();
        second.attach_session_store(path);
        let ok = second.authorize(&headers(&[
            ("cookie", &format!("{}={s}", second.cookie_name())),
            ("sec-fetch-site", "same-origin"),
        ]));
        assert_eq!(ok, Err(Denied::SessionExpired));
    }

    #[test]
    fn stale_session_stores_are_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions-7878.json");
        let now = now_secs();
        let file = serde_json::json!({
            "version": RUNTIME_FILE_VERSION, "port": 7878,
            "saved_at": now - secs(RESTORE_WINDOW) - 1,
            "sessions": [{"secret": "a".repeat(64), "created": now - 10, "last_used": now - 10}],
        });
        std::fs::write(&path, file.to_string()).unwrap();
        let a = auth();
        a.attach_session_store(path);
        assert_eq!(a.view().sessions_active, 0, "the daemon was gone too long");
    }

    #[cfg(unix)]
    #[test]
    fn credentials_are_not_written_into_a_shared_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("run");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = write_private(
            &shared.join("daemon-1.json"),
            b"secret",
            Durability::Unsynced,
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            std::fs::read_dir(&shared).unwrap().next().is_none(),
            "nothing left behind"
        );
    }

    #[test]
    fn expired_or_malformed_stored_sessions_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions-7878.json");
        let now = now_secs();
        let file = serde_json::json!({
            "version": RUNTIME_FILE_VERSION,
            "port": 7878,
            "saved_at": now,
            "sessions": [
                {"secret": "a".repeat(64), "created": now - 10, "last_used": now - 10},
                {"secret": "b".repeat(64), "created": now - 100, "last_used": now - secs(SESSION_IDLE) - 1},
                {"secret": "c".repeat(64), "created": now - secs(SESSION_MAX_AGE) - 1, "last_used": now},
                {"secret": "short", "created": now, "last_used": now},
            ]
        });
        std::fs::write(&path, file.to_string()).unwrap();
        let a = auth();
        a.attach_session_store(path.clone());
        assert_eq!(
            a.view().sessions_active,
            1,
            "only the live, well-formed one"
        );
        // Garbage is not fatal.
        std::fs::write(&path, b"{not json").unwrap();
        let b = auth();
        b.attach_session_store(path);
        assert_eq!(b.view().sessions_active, 0);
    }

    #[test]
    fn runtime_file_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/daemon-7878.json");
        let info = RuntimeInfo {
            version: RUNTIME_FILE_VERSION,
            port: 7878,
            pid: 1,
            instance_id: "i-1".into(),
            url: "http://127.0.0.1:7878".into(),
            token: "t".repeat(64),
            started_at: chrono::Utc::now(),
        };
        write_runtime_file(&path, &info).unwrap();
        let back = read_runtime_file(&path).unwrap();
        assert_eq!(back.token, info.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dmode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dmode, 0o700);
        }
        // Not removed on behalf of another instance.
        remove_runtime_file(&path, "i-2");
        assert!(path.exists());
        remove_runtime_file(&path, "i-1");
        assert!(!path.exists());
    }
}
