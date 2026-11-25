//! Coding-agent integrations (M2).
//!
//! ```text
//!  agent (Claude Code, Codex)
//!     │ documented hook: JSON on stdin / argv
//!     ▼
//!  `tailflow agent hook <agent>`  (short-lived, one per hook call)
//!     │ adapter: validate → normalize → privacy (admit + redact) → bound
//!     ▼
//!  <data dir>/agents/spool/*.json   owner-only files, atomic rename
//!     │ claimed by any running daemon with storage (rename = ownership)
//!     ▼
//!  Storage::submit (privacy again) → SQLite events + agent_sessions
//! ```
//!
//! Rules every adapter follows:
//!
//! - **Observe, never steer.** The hook bridge prints nothing on stdout,
//!   always exits 0, and never touches the database.
//! - **Only claim what the source said.** Each event carries
//!   [`Provenance::AgentReported`]; a `file.modified` from a hook means "the
//!   agent's tool reported an edit", not "TailFlow saw the file change".
//! - **Namespaced identity.** Session keys are `<agent>:<source id>` and event
//!   ids `agent:<agent>:…`, so two agents (or a runtime log) can never collide.
//! - **Replay-safe.** Every spooled event has a fixed id (`stable_id = true`):
//!   a re-delivered spool file is deduplicated by storage. Where the source
//!   supplies identity (tool call id, subagent id) the id is derived from it,
//!   so a duplicated hook registration deduplicates too.

pub mod claude_code;
pub mod codex;
pub mod correlation;
pub mod hook;
pub mod install;
pub mod session;
pub mod spool;

use crate::event::{bounded_id, ObservabilityEvent, Provenance, MAX_ID_LEN};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// `source_type` of every event produced by an agent hook.
pub const SOURCE_TYPE: &str = "agent_hook";

/// Largest hook input read from stdin or argv. Claude Code sends the full
/// `tool_input` (a `Write` carries the whole file), so this is generous; the
/// stored event keeps only metadata.
pub const MAX_HOOK_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// A supported coding agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    pub const ALL: [AgentKind; 2] = [AgentKind::ClaudeCode, AgentKind::Codex];

    /// Stable identifier: the `agent_name` of every event and the session
    /// key namespace.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Codex => "Codex",
        }
    }

    /// Accepts the identifier and common aliases (`claude`, `openai-codex`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude" | "claude_code" | "claudecode" => Some(AgentKind::ClaudeCode),
            "codex" | "openai-codex" | "codex-cli" => Some(AgentKind::Codex),
            _ => None,
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How directly an integration observes a kind of activity. Published in
/// session coverage so a reader can tell "nothing happened" from "TailFlow
/// cannot see this".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageLevel {
    /// A documented event reports each occurrence.
    Direct,
    /// Some occurrences or some fields only.
    Partial,
    /// Not observable through the integration.
    None,
}

/// One row of an agent's capability matrix.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Capability {
    pub activity: &'static str,
    pub level: CoverageLevel,
    pub note: &'static str,
}

/// Per-invocation facts the hook bridge supplies to an adapter.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// When the hook was invoked: the event time for every produced event
    /// (hook payloads carry no timestamps of their own).
    pub received_at: DateTime<Utc>,
    /// Unique per invocation; seeds ids for events without source identity.
    pub nonce: String,
}

impl HookContext {
    pub fn now() -> Self {
        Self {
            received_at: Utc::now(),
            nonce: unique_nonce(),
        }
    }
}

/// Why an adapter produced no events for an input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterError {
    /// Not JSON, or not a JSON object.
    Malformed(String),
    /// A required field is missing or has the wrong type.
    MissingField(&'static str),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdapterError::Malformed(m) => write!(f, "malformed hook input: {m}"),
            AdapterError::MissingField(n) => write!(f, "hook input lacks required field `{n}`"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// The result of normalizing one hook input.
#[derive(Debug, Default)]
pub struct Normalized {
    pub events: Vec<ObservabilityEvent>,
    /// The input was a recognised but intentionally unrecorded event (e.g.
    /// `Notification`), or an event this build does not know. Not loss.
    pub ignored: Option<String>,
}

/// Normalize one hook input for `kind`.
///
/// # Errors
/// The input is not a usable hook payload.
pub fn normalize(
    kind: AgentKind,
    input: &Value,
    ctx: &HookContext,
) -> Result<Normalized, AdapterError> {
    match kind {
        AgentKind::ClaudeCode => claude_code::normalize(input, ctx),
        AgentKind::Codex => codex::normalize(input, ctx),
    }
}

/// The capability matrix of `kind`'s integration.
pub fn capabilities(kind: AgentKind) -> &'static [Capability] {
    match kind {
        AgentKind::ClaudeCode => claude_code::CAPABILITIES,
        AgentKind::Codex => codex::CAPABILITIES,
    }
}

static NONCE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A process-unique, time-ordered nonce: `<micros hex><pid hex><counter>`.
/// Uniqueness (not secrecy) is what ids need; two hook processes never share
/// a pid at the same microsecond.
pub fn unique_nonce() -> String {
    let micros = Utc::now().timestamp_micros();
    let n = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{micros:x}{:x}{n:x}", std::process::id())
}

/// 64-bit FNV-1a. Stable across Rust versions and platforms (unlike
/// `DefaultHasher`), which ids derived from it must be.
pub fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `s` if it is a valid id, otherwise a 128-bit hash of it under `prefix`.
pub(crate) fn fit_id(prefix: &str, s: String) -> String {
    if s.len() <= MAX_ID_LEN && !s.chars().any(char::is_control) && !s.is_empty() {
        return s;
    }
    let b = s.as_bytes();
    format!(
        "{prefix}h:{:016x}{:016x}",
        fnv1a64(b, 0),
        fnv1a64(b, 0x9e37_79b9_7f4a_7c15)
    )
}

/// A source identifier (tool call id, subagent id) if it is short and made of
/// safe characters; `None` otherwise, so it is never used to build an id or a
/// relationship from attacker-shaped text.
pub(crate) fn safe_source_id(v: Option<&Value>) -> Option<&str> {
    let s = v?.as_str()?;
    let ok = !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    ok.then_some(s)
}

/// A short enum-like token from a source (`startup`, `rate_limit`, a model
/// name). Rejects anything that is not a plain identifier.
pub(crate) fn safe_token(v: Option<&Value>, max: usize) -> Option<String> {
    let s = v?.as_str()?;
    let ok = !s.is_empty()
        && s.len() <= max
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '[' | ']')
        });
    ok.then(|| s.to_string())
}

/// Lexically normalize `raw` against `base`: relative paths are joined to
/// `base`, `.` is dropped and `..` pops a component, without touching the
/// filesystem (no symlink resolution — a hook must not stat arbitrary
/// paths, and a path that does not exist yet is still meaningful).
///
/// Returns `None` for an empty path, a relative path with no absolute base,
/// or a `..` that would climb above the root.
pub fn normalize_path(raw: &str, base: Option<&Path>) -> Option<PathBuf> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let p = Path::new(raw);
    let joined: PathBuf = if p.has_root() {
        p.to_path_buf()
    } else {
        let base = base.filter(|b| b.has_root())?;
        base.join(p)
    };
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for comp in joined.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                out.pop();
                depth -= 1;
            }
            Component::Normal(c) => {
                out.push(c);
                depth += 1;
            }
        }
    }
    Some(out)
}

/// `path` relative to `root` for display, when it lies inside it.
pub(crate) fn display_relative(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|r| path.strip_prefix(r).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .map_or_else(
            || path.display().to_string(),
            |rel| rel.display().to_string(),
        )
}

/// What TailFlow records about a shell command by default: the program name
/// and shape, never its arguments (which carry tokens, URLs, passwords).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSummary {
    /// Basename of the first word that is not an environment assignment,
    /// when it is a plain program name.
    pub program: Option<String>,
    /// Contains `&&`, `||`, `;`, `|` or a newline.
    pub compound: bool,
    pub chars: usize,
}

pub fn summarize_command(cmd: &str) -> CommandSummary {
    let compound = cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains(';')
        || cmd.contains('|')
        || cmd.contains('\n');
    let program = cmd
        .split_whitespace()
        .find(|w| !is_env_assignment(w) && *w != "env")
        .map(|w| w.trim_matches(|c| c == '"' || c == '\''))
        .and_then(|w| w.rsplit(['/', '\\']).next())
        .filter(|w| {
            !w.is_empty()
                && w.len() <= 64
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        })
        .map(str::to_string);
    CommandSummary {
        program,
        compound,
        chars: cmd.chars().count(),
    }
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Shared event construction for adapters.
pub(crate) struct EventBuilder<'a> {
    pub kind: AgentKind,
    pub ctx: &'a HookContext,
    pub session_key: Option<String>,
    pub project_id: Option<String>,
    /// Normalized working directory: the `path` of non-file events.
    pub cwd: Option<PathBuf>,
    /// The source event name (`PostToolUse`), kept as `hook.event`.
    pub hook_event: String,
    seq: u32,
}

impl<'a> EventBuilder<'a> {
    pub fn new(kind: AgentKind, ctx: &'a HookContext, hook_event: &str) -> Self {
        Self {
            kind,
            ctx,
            session_key: None,
            project_id: None,
            cwd: None,
            hook_event: hook_event.to_string(),
            seq: 0,
        }
    }

    /// An event of `event_type`. With `derived` the id comes from source
    /// identity (`<session>:<derived>`); otherwise from this invocation's
    /// nonce. Both are fixed once spooled.
    pub fn event(&mut self, event_type: &str, derived: Option<String>) -> ObservabilityEvent {
        let kind = self.kind.as_str();
        let id = match derived {
            Some(d) => format!(
                "agent:{kind}:{}:{d}",
                self.session_key.as_deref().unwrap_or("-")
            ),
            None => {
                self.seq = self.seq.saturating_add(1);
                // The type is part of the id so two contexts that share a
                // nonce (only possible in tests) still cannot collide.
                format!(
                    "agent:{kind}:n:{}:{event_type}:{}",
                    self.ctx.nonce, self.seq
                )
            }
        };
        let mut e = ObservabilityEvent::new(
            fit_id(&format!("agent:{kind}:"), id),
            event_type,
            Provenance::AgentReported,
            SOURCE_TYPE,
            kind,
            self.ctx.received_at,
        );
        e.stable_id = true;
        e.agent_name = Some(kind.to_string());
        e.session_id.clone_from(&self.session_key);
        e.project_id = self.project_id.as_deref().map(bounded_id);
        e.path = self.cwd.as_ref().map(|p| p.display().to_string());
        e.severity = crate::LogLevel::Info;
        e.attributes
            .insert("hook.event".into(), Value::from(self.hook_event.clone()));
        e
    }
}

/// Insert `key` when `value` is `Some`.
pub(crate) fn put(attrs: &mut Map<String, Value>, key: &str, value: Option<impl Into<Value>>) {
    if let Some(v) = value {
        attrs.insert(key.to_string(), v.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_aliases() {
        assert_eq!(AgentKind::parse("claude"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::parse("Claude-Code"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::parse("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::parse("gemini"), None);
        for k in AgentKind::ALL {
            assert_eq!(AgentKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn paths_normalize_lexically() {
        let base = Path::new("/home/u/proj");
        // Compared as paths: Windows renders the same path with `\`.
        let n = |s| normalize_path(s, Some(base));
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(n("src/lib.rs"), p("/home/u/proj/src/lib.rs"));
        assert_eq!(n("./a/../b"), p("/home/u/proj/b"));
        assert_eq!(n("/home/u/proj/../../../etc/passwd"), p("/etc/passwd"));
        assert_eq!(n("/../x"), None, "cannot climb above the root");
        assert_eq!(normalize_path("rel", None), None);
        assert_eq!(n(""), None);
        assert_eq!(n("a\0b"), None);
    }

    #[test]
    fn commands_keep_program_only() {
        let s = summarize_command("API_TOKEN=abc123 FOO=1 /usr/bin/curl -H 'x' https://x");
        assert_eq!(s.program.as_deref(), Some("curl"));
        assert!(!s.compound);
        let s = summarize_command("cd x && cargo test -- --nocapture");
        assert_eq!(s.program.as_deref(), Some("cd"));
        assert!(s.compound);
        assert_eq!(summarize_command("$(evil)").program, None);
        assert_eq!(summarize_command("").program, None);
        assert_eq!(
            summarize_command("env X=1 npm run build")
                .program
                .as_deref(),
            Some("npm")
        );
    }

    #[test]
    fn long_ids_are_hashed_and_valid() {
        let id = fit_id("agent:x:", "a".repeat(400));
        assert!(id.len() <= MAX_ID_LEN);
        assert!(id.starts_with("agent:x:h:"));
        assert_eq!(id, fit_id("agent:x:", "a".repeat(400)), "deterministic");
        assert_ne!(id, fit_id("agent:x:", "b".repeat(400)));
    }

    #[test]
    fn source_ids_reject_unsafe_text() {
        assert_eq!(
            safe_source_id(Some(&Value::from("toolu_01AbC"))),
            Some("toolu_01AbC")
        );
        assert_eq!(safe_source_id(Some(&Value::from("a b"))), None);
        assert_eq!(safe_source_id(Some(&Value::from("x".repeat(129)))), None);
        assert_eq!(safe_source_id(Some(&Value::from(5))), None);
        assert_eq!(safe_source_id(None), None);
    }
}
