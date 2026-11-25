//! Installing and removing TailFlow's agent hooks.
//!
//! Correctness rules (each has a test):
//! 1. **Locate** the agent's own configuration (`~/.claude/settings.json` or
//!    `$CLAUDE_CONFIG_DIR`; `$CODEX_HOME/hooks.json`).
//! 2. **Validate** it: a file that is not a JSON object, or whose `hooks`
//!    has an unexpected shape, is refused, never "repaired".
//! 3. **Preserve** everything else: key order (`serde_json` `preserve_order`),
//!    other hooks, other tools' matcher groups. TailFlow adds its *own*
//!    matcher group per event and never edits someone else's.
//! 4. **No duplicates, idempotent.** A TailFlow handler is recognised by its
//!    shape (`tailflow … agent hook <agent>`); reinstalling updates it in
//!    place (new binary path, timeout) and removes extra copies.
//! 5. **Safe executable reference.** Claude Code gets the exec form
//!    (`command` + `args`, no shell). Codex only has a shell form: the path
//!    is quoted for the platform's hook shell, and agent data never appears
//!    in it.
//! 6. **Uninstall removes only TailFlow handlers**, plus groups and event
//!    arrays that became empty because of it.
//! 7. **Atomic, recoverable writes**: a backup of the previous file, a
//!    temp file + rename in the same directory, the original permissions,
//!    and a symlinked config is updated at its target (dotfile managers keep
//!    working). A read-only file or one changed during the edit is refused.
//! 8. **Policy is reported, not bypassed.** Managed settings that disable
//!    user hooks, and Codex's per-hook trust review, are surfaced by
//!    `agent status`; TailFlow never marks its own hooks as trusted.

use super::{claude_code, codex, AgentKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Largest config file read.
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
/// Hook timeout written to configs (seconds). The bridge returns in
/// milliseconds; this only bounds a wedged filesystem.
pub const HOOK_TIMEOUT_SECS: u64 = 5;
/// Codex caps `SessionEnd` hooks at 3 s.
const CODEX_SESSION_END_TIMEOUT_SECS: u64 = 3;

#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    NotFound(String),
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    Invalid {
        path: PathBuf,
        reason: String,
    },
    ReadOnly(PathBuf),
    ChangedDuringEdit(PathBuf),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::NotFound(m) => f.write_str(m),
            InstallError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            InstallError::Invalid { path, reason } => write!(
                f,
                "{} is not valid ({reason}); it was left unchanged",
                path.display()
            ),
            InstallError::ReadOnly(p) => {
                write!(f, "{} is read-only; it was left unchanged", p.display())
            }
            InstallError::ChangedDuringEdit(p) => write!(
                f,
                "{} changed while TailFlow was editing it; nothing was written, run the command again",
                p.display()
            ),
        }
    }
}

impl std::error::Error for InstallError {}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> InstallError + '_ {
    move |error| InstallError::Io {
        path: path.to_path_buf(),
        error,
    }
}

fn home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where Claude Code settings live for a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeScope {
    /// `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`.
    User,
    /// `<project>/.claude/settings.json` (shared with the team via git).
    Project(PathBuf),
    /// `<project>/.claude/settings.local.json` (git-ignored by Claude Code).
    Local(PathBuf),
}

pub fn claude_settings_path(scope: &ClaudeScope) -> Option<PathBuf> {
    match scope {
        ClaudeScope::User => std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".claude")))
            .map(|d| d.join("settings.json")),
        ClaudeScope::Project(p) => Some(p.join(".claude").join("settings.json")),
        ClaudeScope::Local(p) => Some(p.join(".claude").join("settings.local.json")),
    }
}

/// `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`.
pub fn codex_hooks_path() -> Option<PathBuf> {
    codex_home().map(|d| d.join("hooks.json"))
}

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
}

/// Claude Code managed (policy) settings, by platform. Best effort: used
/// only to warn that policy disables user hooks.
pub fn claude_managed_settings_paths() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![PathBuf::from(
            "/Library/Application Support/ClaudeCode/managed-settings.json",
        )]
    } else if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json"),
            PathBuf::from(r"C:\ProgramData\ClaudeCode\managed-settings.json"),
        ]
    } else {
        vec![PathBuf::from("/etc/claude-code/managed-settings.json")]
    }
}

/// POSIX single-quote `s` for `sh -c`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The handler TailFlow wants for `event`.
fn desired_handler(kind: AgentKind, exe: &Path, event: &str) -> Value {
    let exe_s = exe.display().to_string();
    match kind {
        AgentKind::ClaudeCode => json!({
            "type": "command",
            "command": exe_s,
            "args": ["agent", "hook", kind.as_str()],
            "timeout": HOOK_TIMEOUT_SECS,
        }),
        AgentKind::Codex => {
            // Codex runs `$SHELL -lc <command>` (Unix) or `cmd /C "<command>"`
            // (Windows). Windows paths cannot contain `"`.
            let quoted = if cfg!(windows) {
                format!("\"{exe_s}\"")
            } else {
                sh_quote(&exe_s)
            };
            let timeout = if event == "SessionEnd" {
                CODEX_SESSION_END_TIMEOUT_SECS
            } else {
                HOOK_TIMEOUT_SECS
            };
            json!({
                "type": "command",
                "command": format!("{quoted} agent hook {}", kind.as_str()),
                "timeout": timeout,
            })
        }
    }
}

fn is_tailflow_exe(s: &str) -> bool {
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let lower = base.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower) == "tailflow"
}

/// Split the first shell word off `s`: a bare word, or one built from
/// `'…'` / `"…"` segments (and the `'\''` sequence `sh_quote` emits).
/// Returns `(word, rest)`; `None` for unbalanced quotes or a word that
/// contains shell metacharacters outside quotes.
fn first_shell_word(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let mut word = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        match c {
            '\'' | '"' => {
                chars.next();
                let mut closed = false;
                for (_, d) in chars.by_ref() {
                    if d == c {
                        closed = true;
                        break;
                    }
                    word.push(d);
                }
                if !closed {
                    return None;
                }
            }
            '\\' if s.get(i + 1..i + 2) == Some("'") => {
                chars.next();
                chars.next();
                word.push('\'');
            }
            c if c.is_whitespace() => return Some((word, s.get(i..).unwrap_or(""))),
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '$' | '`' | '=' => return None,
            _ => {
                chars.next();
                word.push(c);
            }
        }
    }
    Some((word, ""))
}

/// The executable a TailFlow handler for `kind` runs, if `h` is one.
///
/// Recognized shapes only: `{"type":"command","command":"<…/tailflow>",
/// "args":["agent","hook",<agent>]}` (exec form), or a `command` that is a
/// single, possibly quoted, `tailflow` executable followed by exactly
/// `agent hook <agent>` (shell form). A user's wrapper (`env X=1 tailflow …`,
/// `log.sh && tailflow …`) is *not* TailFlow's and is never edited.
pub fn owned_handler_exe(kind: AgentKind, h: &Value) -> Option<String> {
    let obj = h.as_object()?;
    if obj.get("type").and_then(Value::as_str) != Some("command") {
        return None;
    }
    let command = obj.get("command").and_then(Value::as_str)?;
    if let Some(args) = obj.get("args").and_then(Value::as_array) {
        let args: Vec<&str> = args.iter().filter_map(Value::as_str).collect();
        let ok = args.len() == 3
            && args.first() == Some(&"agent")
            && args.get(1) == Some(&"hook")
            && args.get(2).and_then(|a| AgentKind::parse(a)) == Some(kind)
            && is_tailflow_exe(command);
        return ok.then(|| command.to_string());
    }
    let (exe, rest) = first_shell_word(command)?;
    let rest: Vec<&str> = rest.split_whitespace().collect();
    let ok = rest.len() == 3
        && rest.first() == Some(&"agent")
        && rest.get(1) == Some(&"hook")
        && rest.get(2).and_then(|a| AgentKind::parse(a)) == Some(kind)
        && is_tailflow_exe(&exe);
    ok.then_some(exe)
}

/// Whether `h` is a TailFlow handler for `kind` (any version, any path).
pub fn is_owned_handler(kind: AgentKind, h: &Value) -> bool {
    owned_handler_exe(kind, h).is_some()
}

/// The hook events TailFlow registers for `kind`. For Codex, only events
/// the given version supports (`None` = version unknown → the set every
/// stable-hooks release supports).
pub fn events_for(kind: AgentKind, codex_version: Option<(u32, u32, u32)>) -> Vec<&'static str> {
    match kind {
        AgentKind::ClaudeCode => claude_code::HOOK_EVENTS.to_vec(),
        AgentKind::Codex => {
            let v = codex_version.unwrap_or(codex::MIN_VERSION);
            codex::HOOK_EVENTS
                .iter()
                .filter(|(_, since)| v >= *since)
                .map(|(e, _)| *e)
                .collect()
        }
    }
}

/// A computed edit. `apply` writes it.
#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: AgentKind,
    pub path: PathBuf,
    /// The file as read (`None` = did not exist).
    pub before: Option<String>,
    pub after: Value,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub removed: usize,
}

impl Plan {
    pub fn changes(&self) -> bool {
        !self.added.is_empty() || !self.updated.is_empty() || self.removed > 0
    }

    /// The new file text.
    pub fn render(&self) -> String {
        let mut s = serde_json::to_string_pretty(&self.after).unwrap_or_else(|_| "{}".into());
        s.push('\n');
        s
    }
}

/// Read a JSON config: `None` when absent; an empty file is `{}`.
fn read_config(path: &Path) -> Result<Option<(String, Value)>, InstallError> {
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(path)(e)),
    };
    let mut text = String::new();
    f.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(io_err(path))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
        return Err(InstallError::Invalid {
            path: path.into(),
            reason: "larger than 4 MiB".into(),
        });
    }
    if text.trim().is_empty() {
        return Ok(Some((text, Value::Object(Map::new()))));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| InstallError::Invalid {
        path: path.into(),
        reason: format!("not JSON: {e}"),
    })?;
    if !v.is_object() {
        return Err(InstallError::Invalid {
            path: path.into(),
            reason: "the top level must be a JSON object".into(),
        });
    }
    Ok(Some((text, v)))
}

fn hooks_obj<'a>(
    root: &'a mut Value,
    path: &Path,
    create: bool,
) -> Result<Option<&'a mut Map<String, Value>>, InstallError> {
    let obj = root.as_object_mut().ok_or_else(|| InstallError::Invalid {
        path: path.into(),
        reason: "the top level must be a JSON object".into(),
    })?;
    if !obj.contains_key("hooks") {
        if !create {
            return Ok(None);
        }
        obj.insert("hooks".into(), Value::Object(Map::new()));
    }
    match obj.get_mut("hooks") {
        Some(Value::Object(h)) => Ok(Some(h)),
        _ => Err(InstallError::Invalid {
            path: path.into(),
            reason: "`hooks` must be an object".into(),
        }),
    }
}

fn groups_of<'a>(
    hooks: &'a mut Map<String, Value>,
    event: &str,
    path: &Path,
) -> Result<Option<&'a mut Vec<Value>>, InstallError> {
    match hooks.get_mut(event) {
        None => Ok(None),
        Some(Value::Array(a)) => Ok(Some(a)),
        Some(_) => Err(InstallError::Invalid {
            path: path.into(),
            reason: format!("`hooks.{event}` must be an array"),
        }),
    }
}

/// Plan installing `kind`'s hooks for `events` into `path`, pointing at `exe`.
///
/// # Errors
/// The file cannot be read or has an unexpected shape.
pub fn plan_install(
    kind: AgentKind,
    path: &Path,
    exe: &Path,
    events: &[&str],
) -> Result<Plan, InstallError> {
    let read = read_config(path)?;
    let before = read.as_ref().map(|(t, _)| t.clone());
    let mut root = read.map_or_else(|| Value::Object(Map::new()), |(_, v)| v);
    let mut plan = Plan {
        kind,
        path: path.to_path_buf(),
        before,
        after: Value::Null,
        added: Vec::new(),
        updated: Vec::new(),
        unchanged: Vec::new(),
        removed: 0,
    };
    {
        let hooks = hooks_obj(&mut root, path, true)?
            .ok_or_else(|| InstallError::NotFound("hooks".into()))?;
        for event in events {
            let want = desired_handler(kind, exe, event);
            if !hooks.contains_key(*event) {
                hooks.insert((*event).to_string(), Value::Array(Vec::new()));
            }
            let groups = groups_of(hooks, event, path)?
                .ok_or_else(|| InstallError::NotFound((*event).to_string()))?;
            // Keep the copy in TailFlow's own group (one holding only TailFlow
            // handlers) when there is one; copies elsewhere are duplicates.
            let list_of = |g: &Value| g.get("hooks").and_then(Value::as_array).cloned();
            let preferred = groups
                .iter()
                .position(|g| {
                    list_of(g).is_some_and(|l| {
                        !l.is_empty() && l.iter().all(|h| is_owned_handler(kind, h))
                    })
                })
                .or_else(|| {
                    groups.iter().position(|g| {
                        list_of(g).is_some_and(|l| l.iter().any(|h| is_owned_handler(kind, h)))
                    })
                });
            let mut found = false;
            let mut changed = false;
            let mut emptied = vec![false; groups.len()];
            for (gi, group) in groups.iter_mut().enumerate() {
                let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                    continue;
                };
                let before_len = list.len();
                let mut keep = Vec::with_capacity(list.len());
                for h in list.drain(..) {
                    if !is_owned_handler(kind, &h) {
                        keep.push(h);
                    } else if Some(gi) == preferred && !found {
                        found = true;
                        if h == want {
                            keep.push(h);
                        } else {
                            changed = true;
                            keep.push(want.clone());
                        }
                    } else {
                        plan.removed += 1; // duplicate copy
                        changed = true;
                    }
                }
                *list = keep;
                if let Some(slot) = emptied.get_mut(gi) {
                    *slot = before_len > 0 && list.is_empty();
                }
            }
            // Drop only groups this edit emptied (they held duplicates).
            let mut gi = 0usize;
            groups.retain(|_| {
                let keep = !emptied.get(gi).copied().unwrap_or(false);
                gi += 1;
                keep
            });
            if !found {
                groups.push(json!({ "hooks": [want] }));
                plan.added.push((*event).to_string());
            } else if changed {
                plan.updated.push((*event).to_string());
            } else {
                plan.unchanged.push((*event).to_string());
            }
        }
    }
    plan.after = root;
    Ok(plan)
}

/// Plan removing every TailFlow handler for `kind` from `path`.
///
/// # Errors
/// The file cannot be read or has an unexpected shape.
pub fn plan_uninstall(kind: AgentKind, path: &Path) -> Result<Plan, InstallError> {
    let read = read_config(path)?;
    let Some((text, mut root)) = read else {
        return Ok(Plan {
            kind,
            path: path.to_path_buf(),
            before: None,
            after: Value::Null,
            added: Vec::new(),
            updated: Vec::new(),
            unchanged: Vec::new(),
            removed: 0,
        });
    };
    let mut removed = 0usize;
    let mut emptied_hooks = false;
    if let Some(hooks) = hooks_obj(&mut root, path, false)? {
        let had_any = !hooks.is_empty();
        let events: Vec<String> = hooks.keys().cloned().collect();
        for event in events {
            let Some(groups) = groups_of(hooks, &event, path)? else {
                continue;
            };
            let mut touched = false;
            for group in groups.iter_mut() {
                if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                    let n = list.len();
                    list.retain(|h| !is_owned_handler(kind, h));
                    if list.len() != n {
                        removed += n - list.len();
                        touched = true;
                    }
                }
            }
            if touched {
                groups.retain(|g| {
                    g.get("hooks")
                        .and_then(Value::as_array)
                        .is_none_or(|l| !l.is_empty())
                });
                if groups.is_empty() {
                    hooks.remove(&event);
                }
            }
        }
        emptied_hooks = had_any && hooks.is_empty() && removed > 0;
    }
    if emptied_hooks {
        if let Some(obj) = root.as_object_mut() {
            obj.remove("hooks");
        }
    }
    Ok(Plan {
        kind,
        path: path.to_path_buf(),
        before: Some(text),
        after: root,
        added: Vec::new(),
        updated: Vec::new(),
        unchanged: Vec::new(),
        removed,
    })
}

/// Write a plan: backup, temp file, rename. No-op when nothing changes.
/// Returns the backup path, if one was made.
///
/// # Errors
/// Read-only target, concurrent modification, or an I/O failure (the
/// original file is untouched in every error case).
pub fn apply(plan: &Plan) -> Result<Option<PathBuf>, InstallError> {
    if !plan.changes() {
        return Ok(None);
    }
    // Update a symlinked config at its target.
    let target = match fs::canonicalize(&plan.path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => plan.path.clone(),
        Err(e) => return Err(io_err(&plan.path)(e)),
    };
    let dir = target
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| InstallError::NotFound(format!("{} has no parent", target.display())))?;
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    let meta = match fs::metadata(&target) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err(&target)(e)),
    };
    if meta.as_ref().is_some_and(|m| m.permissions().readonly()) {
        return Err(InstallError::ReadOnly(target));
    }
    // Re-read just before writing: refuse if someone else changed it.
    let current = read_config(&target)?.map(|(t, _)| t);
    if current != plan.before {
        return Err(InstallError::ChangedDuringEdit(target));
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into());
    let backup = if let Some(before) = &plan.before {
        let b = dir.join(format!("{name}.tailflow-backup"));
        write_new(&b, before.as_bytes(), meta.as_ref(), true)?;
        Some(b)
    } else {
        None
    };
    let tmp = dir.join(format!(".{name}.tailflow-tmp-{}", super::unique_nonce()));
    if let Err(e) = write_new(&tmp, plan.render().as_bytes(), meta.as_ref(), false) {
        let _cleanup = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        let _cleanup = fs::remove_file(&tmp);
        return Err(io_err(&target)(e));
    }
    Ok(backup)
}

fn write_new(
    path: &Path,
    bytes: &[u8],
    like: Option<&fs::Metadata>,
    replace: bool,
) -> Result<(), InstallError> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true);
    if replace {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(like.map_or(0o600, |m| m.permissions().mode() & 0o777));
    }
    #[cfg(not(unix))]
    let _ = like;
    let mut f = opts.open(path).map_err(io_err(path))?;
    f.write_all(bytes).map_err(io_err(path))?;
    f.sync_all().map_err(io_err(path))?;
    Ok(())
}

/// What is installed in a config file.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Inspection {
    pub path: PathBuf,
    pub exists: bool,
    /// Events with a TailFlow handler.
    pub installed: Vec<String>,
    /// Expected events without one.
    pub missing: Vec<String>,
    /// Events with more than one TailFlow handler.
    pub duplicated: Vec<String>,
    /// Handlers pointing at a different TailFlow binary than this one.
    pub other_binary: Vec<String>,
    /// Handler executables that do not exist (moved, upgraded away): these
    /// hooks fail to start and nothing is recorded.
    pub missing_binaries: Vec<String>,
    /// `disableAllHooks: true` in this file.
    pub hooks_disabled: bool,
    pub error: Option<String>,
}

/// Inspect `path` for `kind`'s hooks against the expected `events`.
pub fn inspect(kind: AgentKind, path: &Path, exe: &Path, events: &[&str]) -> Inspection {
    let mut out = Inspection {
        path: path.to_path_buf(),
        ..Inspection::default()
    };
    let (_, root) = match read_config(path) {
        Ok(Some(r)) => {
            out.exists = true;
            r
        }
        Ok(None) => {
            out.missing = events.iter().map(|e| (*e).to_string()).collect();
            return out;
        }
        Err(e) => {
            out.exists = path.exists();
            out.error = Some(e.to_string());
            return out;
        }
    };
    out.hooks_disabled = root.get("disableAllHooks").and_then(Value::as_bool) == Some(true);
    let hooks = root.get("hooks").and_then(Value::as_object);
    for event in events {
        let mut owned: Vec<&Value> = Vec::new();
        if let Some(groups) = hooks.and_then(|h| h.get(*event)).and_then(Value::as_array) {
            for g in groups {
                if let Some(list) = g.get("hooks").and_then(Value::as_array) {
                    owned.extend(list.iter().filter(|h| is_owned_handler(kind, h)));
                }
            }
        }
        match owned.len() {
            0 => out.missing.push((*event).to_string()),
            n => {
                out.installed.push((*event).to_string());
                if n > 1 {
                    out.duplicated.push((*event).to_string());
                }
                if owned
                    .iter()
                    .any(|h| **h != desired_handler(kind, exe, event))
                {
                    out.other_binary.push((*event).to_string());
                }
                for h in &owned {
                    if let Some(bin) = owned_handler_exe(kind, h) {
                        if !Path::new(&bin).exists() && !out.missing_binaries.contains(&bin) {
                            out.missing_binaries.push(bin);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Whether Claude Code managed policy blocks user hooks. Returns the policy
/// file and the setting that does it.
pub fn claude_policy_blocks_hooks() -> Option<(PathBuf, &'static str)> {
    for p in claude_managed_settings_paths() {
        let Ok(Some((_, v))) = read_config(&p) else {
            continue;
        };
        if v.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
            return Some((p, "disableAllHooks"));
        }
        if v.get("allowManagedHooksOnly").and_then(Value::as_bool) == Some(true) {
            return Some((p, "allowManagedHooksOnly"));
        }
    }
    None
}

/// Codex settings that stop hooks from running, read from `config.toml`.
pub fn codex_config_warnings() -> Vec<String> {
    let mut out = Vec::new();
    let Some(path) = codex_home().map(|h| h.join("config.toml")) else {
        return out;
    };
    let Ok(f) = fs::File::open(&path) else {
        return out;
    };
    let mut text = String::new();
    if f.take(MAX_CONFIG_BYTES).read_to_string(&mut text).is_err() {
        return out;
    }
    let Ok(v) = text.parse::<toml::Table>() else {
        out.push(format!("{} could not be parsed", path.display()));
        return out;
    };
    let features = v.get("features").and_then(toml::Value::as_table);
    for key in ["hooks", "codex_hooks"] {
        if features
            .and_then(|f| f.get(key))
            .and_then(toml::Value::as_bool)
            == Some(false)
        {
            out.push(format!(
                "{} sets [features] {key} = false: Codex runs no hooks",
                path.display()
            ));
        }
    }
    if v.contains_key("hooks") {
        out.push(format!(
            "{} also declares inline [hooks]; Codex loads both and warns at startup",
            path.display()
        ));
    }
    out
}

/// Run `codex --version` (bounded to 5 s) and parse it.
pub fn detect_codex_version() -> Option<(u32, u32, u32)> {
    let mut child = std::process::Command::new(if cfg!(windows) { "codex.cmd" } else { "codex" })
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .or_else(|_| {
            std::process::Command::new("codex")
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        })
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _kill_best_effort = child.kill();
                let _reap = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child
        .stdout
        .take()?
        .take(4096)
        .read_to_string(&mut out)
        .ok()?;
    codex::parse_version(&out)
}

/// One installed integration, as recorded in `integrations.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationRecord {
    pub config_path: PathBuf,
    pub binary: PathBuf,
    pub events: Vec<String>,
    pub installed_at: chrono::DateTime<chrono::Utc>,
    pub tailflow_version: String,
}

/// `integrations.json`: what `agent install` did, for `agent status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationState {
    #[serde(default)]
    pub integrations: std::collections::BTreeMap<AgentKind, Vec<IntegrationRecord>>,
}

impl IntegrationState {
    pub fn path(agents_dir: &Path) -> PathBuf {
        agents_dir.join("integrations.json")
    }

    /// # Errors
    /// Unreadable or invalid state file (a missing file is empty state).
    pub fn load(agents_dir: &Path) -> Result<Self, InstallError> {
        let p = Self::path(agents_dir);
        match read_config(&p)? {
            None => Ok(Self::default()),
            Some((_, v)) => serde_json::from_value(v).map_err(|e| InstallError::Invalid {
                path: p,
                reason: e.to_string(),
            }),
        }
    }

    /// # Errors
    /// The state file cannot be written.
    pub fn save(&self, agents_dir: &Path) -> Result<(), InstallError> {
        crate::paths::create_private_dir(agents_dir).map_err(io_err(agents_dir))?;
        let p = Self::path(agents_dir);
        let tmp = agents_dir.join(format!(".integrations.json.tmp-{}", super::unique_nonce()));
        let mut text = serde_json::to_string_pretty(self).map_err(|e| InstallError::Invalid {
            path: p.clone(),
            reason: e.to_string(),
        })?;
        text.push('\n');
        if let Err(e) = write_new(&tmp, text.as_bytes(), None, false) {
            let _cleanup = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::rename(&tmp, &p).map_err(io_err(&p))
    }

    pub fn record(&mut self, kind: AgentKind, rec: IntegrationRecord) {
        let list = self.integrations.entry(kind).or_default();
        list.retain(|r| r.config_path != rec.config_path);
        list.push(rec);
    }

    pub fn forget(&mut self, kind: AgentKind, config_path: &Path) {
        if let Some(list) = self.integrations.get_mut(&kind) {
            list.retain(|r| r.config_path != config_path);
            if list.is_empty() {
                self.integrations.remove(&kind);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/tail flow/bin/tailflow";

    fn exe() -> PathBuf {
        PathBuf::from(EXE)
    }

    /// A settings file shaped like a real one: other tools' hooks with
    /// private keys, unrelated settings, deliberate key order.
    fn existing() -> &'static str {
        r#"{
  "model": "opus",
  "hooks": {
    "SessionStart": [
      {
        "hooks": [ { "type": "command", "command": "/x/gstack-session-update" } ],
        "_gstack_source": "gstack-session-update"
      }
    ],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "/x/guard", "timeout": 10 } ] }
    ]
  },
  "permissions": { "allow": ["Bash(ls)"] },
  "zeta": 1,
  "alpha": 2
}"#
    }

    fn file(content: &str) -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        fs::write(&p, content).unwrap();
        (t, p)
    }

    fn reparse(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn install_preserves_and_uninstall_restores() {
        let (_t, p) = file(existing());
        let original: Value = serde_json::from_str(existing()).unwrap();
        let events = events_for(AgentKind::ClaudeCode, None);
        let plan = plan_install(AgentKind::ClaudeCode, &p, &exe(), &events).unwrap();
        assert_eq!(plan.added.len(), events.len());
        let backup = apply(&plan).unwrap().unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), existing());

        let after = reparse(&p);
        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["model", "hooks", "permissions", "zeta", "alpha"],
            "key order kept"
        );
        assert_eq!(
            after["hooks"]["SessionStart"][0],
            original["hooks"]["SessionStart"][0]
        );
        assert_eq!(
            after["hooks"]["PreToolUse"][0],
            original["hooks"]["PreToolUse"][0]
        );
        let ours = &after["hooks"]["PreToolUse"][1]["hooks"][0];
        assert_eq!(
            ours["command"], EXE,
            "exec form: the path is not shell-quoted"
        );
        assert_eq!(ours["args"], json!(["agent", "hook", "claude-code"]));
        assert!(
            after["hooks"]["PreToolUse"][1].get("matcher").is_none(),
            "matches every tool"
        );

        // Idempotent.
        let again = plan_install(AgentKind::ClaudeCode, &p, &exe(), &events).unwrap();
        assert!(!again.changes());
        assert_eq!(apply(&again).unwrap(), None);

        let un = plan_uninstall(AgentKind::ClaudeCode, &p).unwrap();
        assert_eq!(un.removed, events.len());
        apply(&un).unwrap();
        assert_eq!(
            reparse(&p),
            original,
            "uninstall restores the original document"
        );
    }

    #[test]
    fn reinstall_updates_in_place_and_removes_duplicates() {
        let (_t, p) = file(existing());
        let events = ["PreToolUse"];
        apply(
            &plan_install(
                AgentKind::ClaudeCode,
                &p,
                Path::new("/old/tailflow"),
                &events,
            )
            .unwrap(),
        )
        .unwrap();
        // A hand-copied duplicate inside another group.
        let mut v = reparse(&p);
        v["hooks"]["PreToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"command","command":"/old/tailflow","args":["agent","hook","claude"]}));
        fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let ins = inspect(AgentKind::ClaudeCode, &p, &exe(), &events);
        assert_eq!(ins.duplicated, ["PreToolUse"]);
        assert_eq!(ins.other_binary, ["PreToolUse"]);

        let plan = plan_install(AgentKind::ClaudeCode, &p, &exe(), &events).unwrap();
        assert_eq!(plan.updated, ["PreToolUse"]);
        apply(&plan).unwrap();
        let ins = inspect(AgentKind::ClaudeCode, &p, &exe(), &events);
        assert!(ins.duplicated.is_empty() && ins.other_binary.is_empty() && ins.missing.is_empty());
        let v = reparse(&p);
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "guard kept"
        );
    }

    #[test]
    fn foreign_handlers_are_never_claimed() {
        for h in [
            json!({"type":"command","command":"/x/toolwake hook"}),
            json!({"type":"command","command":"/x/not-tailflow","args":["agent","hook","claude-code"]}),
            json!({"type":"command","command":"/x/tailflow","args":["agent","hook","codex"]}),
            json!({"type":"command","command":"echo tailflow agent hook claude-code-ish"}),
            json!({"type":"http","url":"http://x"}),
        ] {
            assert!(!is_owned_handler(AgentKind::ClaudeCode, &h), "{h}");
        }
        assert!(is_owned_handler(
            AgentKind::Codex,
            &json!({"type":"command","command":"'/a b/tailflow' agent hook codex"})
        ));
        assert!(is_owned_handler(
            AgentKind::Codex,
            &json!({"type":"command","command":"\"C:\\T F\\tailflow.exe\" agent hook codex"})
        ));
    }

    #[test]
    fn user_wrappers_around_tailflow_are_not_owned() {
        for cmd in [
            "TAILFLOW_DATA_DIR=/x /usr/bin/tailflow agent hook codex",
            "log.sh && ~/bin/tailflow agent hook codex",
            "/usr/bin/tailflow agent hook codex --extra",
            "'/a b/tailflow agent hook codex",
            "$(which tailflow) agent hook codex",
        ] {
            let h = json!({"type":"command","command":cmd});
            assert!(!is_owned_handler(AgentKind::Codex, &h), "{cmd}");
        }
        assert!(!is_owned_handler(
            AgentKind::Codex,
            &json!({"type":"http","command":"tailflow agent hook codex"})
        ));
        let quoted = json!({"type":"command","command":sh_quote("/it's here/tailflow") + " agent hook codex"});
        assert_eq!(
            owned_handler_exe(AgentKind::Codex, &quoted).as_deref(),
            Some("/it's here/tailflow")
        );
        let upper = json!({"type":"command","command":"C:\\T\\TailFlow.EXE","args":["agent","hook","claude"]});
        assert!(
            is_owned_handler(AgentKind::ClaudeCode, &upper),
            "case-insensitive .exe"
        );
    }

    #[test]
    fn a_missing_hook_binary_is_reported() {
        let (_t, p) = file(existing());
        let plan = plan_install(
            AgentKind::ClaudeCode,
            &p,
            Path::new("/nonexistent/tailflow"),
            &["Stop"],
        );
        apply(&plan.unwrap()).unwrap();
        let ins = inspect(AgentKind::ClaudeCode, &p, &exe(), &["Stop"]);
        assert_eq!(ins.missing_binaries, ["/nonexistent/tailflow"]);
    }

    #[test]
    fn invalid_and_unexpected_files_are_refused_untouched() {
        for bad in [
            "{not json",
            "[1,2]",
            r#"{"hooks": []}"#,
            r#"{"hooks": {"PreToolUse": {}}}"#,
        ] {
            let (_t, p) = file(bad);
            let err = plan_install(AgentKind::ClaudeCode, &p, &exe(), &["PreToolUse"]).unwrap_err();
            assert!(matches!(err, InstallError::Invalid { .. }), "{bad}: {err}");
            assert_eq!(fs::read_to_string(&p).unwrap(), bad);
        }
    }

    #[test]
    fn missing_and_empty_files_are_created() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/hooks.json");
        let plan = plan_install(
            AgentKind::Codex,
            &p,
            &exe(),
            &events_for(AgentKind::Codex, None),
        )
        .unwrap();
        assert_eq!(
            apply(&plan).unwrap(),
            None,
            "no backup of a file that did not exist"
        );
        let v = reparse(&p);
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        if !cfg!(windows) {
            assert_eq!(
                cmd, "'/opt/tail flow/bin/tailflow' agent hook codex",
                "shell-quoted"
            );
        }
        let (_t2, empty) = file("");
        assert!(
            plan_install(AgentKind::ClaudeCode, &empty, &exe(), &["Stop"])
                .unwrap()
                .changes()
        );
    }

    #[test]
    fn concurrent_modification_is_detected() {
        let (_t, p) = file(existing());
        let plan = plan_install(AgentKind::ClaudeCode, &p, &exe(), &["Stop"]).unwrap();
        fs::write(&p, r#"{"changed": true}"#).unwrap();
        assert!(matches!(
            apply(&plan),
            Err(InstallError::ChangedDuringEdit(_))
        ));
        assert_eq!(fs::read_to_string(&p).unwrap(), r#"{"changed": true}"#);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_files_are_refused_and_symlinks_followed() {
        use std::os::unix::fs::PermissionsExt;
        let (t, p) = file(existing());
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        let plan = plan_install(AgentKind::ClaudeCode, &p, &exe(), &["Stop"]).unwrap();
        assert!(matches!(apply(&plan), Err(InstallError::ReadOnly(_))));
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();

        let link = t.path().join("link.json");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        apply(&plan_install(AgentKind::ClaudeCode, &link, &exe(), &["Stop"]).unwrap()).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link kept"
        );
        assert!(reparse(&p)["hooks"]["Stop"].is_array(), "target updated");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640,
            "mode kept"
        );
    }

    #[test]
    fn codex_events_follow_the_installed_version() {
        assert_eq!(events_for(AgentKind::Codex, Some((0, 124, 0))).len(), 5);
        assert!(!events_for(AgentKind::Codex, None).contains(&"PostModelRequest"));
        let all = events_for(AgentKind::Codex, Some((0, 172, 0)));
        assert!(all.contains(&"PostModelRequest") && all.contains(&"SessionEnd"));
        assert!(events_for(AgentKind::Codex, Some((0, 100, 0))).is_empty());
    }

    #[test]
    fn sh_quote_handles_quotes() {
        assert_eq!(sh_quote("/a'b"), r"'/a'\''b'");
    }

    #[test]
    fn state_round_trips() {
        let t = tempfile::tempdir().unwrap();
        let mut s = IntegrationState::default();
        s.record(
            AgentKind::Codex,
            IntegrationRecord {
                config_path: "/h/.codex/hooks.json".into(),
                binary: exe(),
                events: vec!["Stop".into()],
                installed_at: chrono::Utc::now(),
                tailflow_version: "0".into(),
            },
        );
        s.save(t.path()).unwrap();
        assert_eq!(IntegrationState::load(t.path()).unwrap(), s);
        s.forget(AgentKind::Codex, Path::new("/h/.codex/hooks.json"));
        assert!(s.integrations.is_empty());
    }
}
