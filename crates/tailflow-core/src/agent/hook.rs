//! The hook bridge: what `tailflow agent hook <agent>` does for one call.
//!
//! Contract with the agent (both Claude Code and Codex):
//! - **stdout stays empty.** Both agents inject a `SessionStart` /
//!   `UserPromptSubmit` hook's stdout into the model's context.
//! - **Exit code is always 0**, so the agent never treats TailFlow as
//!   blocking (exit 2) or failing. Problems are recorded as loss in the
//!   spool and shown by `tailflow agent status`, not printed.
//! - **Bounded work.** Input is capped at [`MAX_HOOK_INPUT_BYTES`]; no
//!   database, no network, no child processes; Git metadata is read only on
//!   session boundaries.
//!
//! Privacy runs here, *before* anything is written to disk, using the
//! per-user agent capture config (`<data dir>/agents/config.toml`,
//! `[privacy]` with the same keys as `tailflow.toml`). The ingesting daemon
//! applies its own `[privacy]` policy again on top: the stricter one wins.

use super::spool::{RejectReason, Spool, SpoolRecord, WriteOutcome};
use super::{normalize, AgentKind, HookContext, MAX_HOOK_INPUT_BYTES};
use crate::privacy::{CapturePolicy, PrivacyConfig};
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};

/// `<data dir>/agents/config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub privacy: PrivacyConfig,
}

pub fn config_path(agents_dir: &Path) -> PathBuf {
    agents_dir.join("config.toml")
}

/// Load the per-user agent config. A missing file means defaults
/// (`metadata` capture, built-in redaction).
///
/// # Errors
/// Unreadable, unparsable, or an invalid policy (unknown rule, bad glob).
pub fn load_policy(agents_dir: &Path) -> Result<CapturePolicy, String> {
    let path = config_path(agents_dir);
    let cfg = match std::fs::File::open(&path) {
        Ok(f) => {
            let mut text = String::new();
            f.take(1024 * 1024)
                .read_to_string(&mut text)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            toml::from_str::<AgentConfig>(&text).map_err(|e| format!("{}: {e}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AgentConfig::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    // Lazy: a hook process redacts a handful of short strings; compiling
    // every rule up front would dominate its run time.
    CapturePolicy::from_config_lazy(&cfg.privacy).map_err(|e| format!("{}: {e}", path.display()))
}

/// What one hook call did (for tests and `TAILFLOW_HOOK_DEBUG`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub events: usize,
    pub excluded: u32,
    pub rejected: Vec<RejectReason>,
    pub ignored: bool,
    pub outcome: Result<WriteOutcome, String>,
}

/// Read at most `cap` bytes; then drain (and discard) a bounded remainder so
/// the agent's write does not fail with a broken pipe. Returns the bytes and
/// whether the input exceeded `cap`.
fn read_bounded(input: &mut impl Read, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    input.by_ref().take(limit).read_to_end(&mut buf)?;
    if buf.len() <= cap {
        return Ok((buf, false));
    }
    buf.clear();
    buf.shrink_to_fit();
    let mut sink = [0u8; 64 * 1024];
    let mut drained = 0usize;
    while drained < 8 * cap {
        match input.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained = drained.saturating_add(n),
        }
    }
    Ok((Vec::new(), true))
}

/// Process one hook call: read `input`, normalize, apply `policy`, spool.
pub fn run(
    kind: AgentKind,
    input: &mut impl Read,
    policy: Result<&CapturePolicy, &str>,
    spool: &Spool,
) -> HookReport {
    let ctx = HookContext::now();
    let mut rec = SpoolRecord::new(kind);
    rec.written_at = ctx.received_at;
    let mut report = HookReport {
        events: 0,
        excluded: 0,
        rejected: Vec::new(),
        ignored: false,
        outcome: Ok(WriteOutcome::Empty),
    };
    let mut reject = |rec: &mut SpoolRecord, r: RejectReason, n: u32| {
        rec.reject(r, n);
        report.rejected.push(r);
    };

    match read_bounded(input, MAX_HOOK_INPUT_BYTES) {
        Ok((_, true)) => reject(&mut rec, RejectReason::OversizedInput, 1),
        Err(_) => reject(&mut rec, RejectReason::MalformedInput, 1),
        Ok((bytes, false)) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Err(_) => reject(&mut rec, RejectReason::MalformedInput, 1),
            Ok(value) => match normalize(kind, &value, &ctx) {
                Err(_) => reject(&mut rec, RejectReason::MalformedInput, 1),
                Ok(normalized) => {
                    report.ignored = normalized.ignored.is_some();
                    let n = u32::try_from(normalized.events.len()).unwrap_or(u32::MAX);
                    match policy {
                        Err(_) => reject(&mut rec, RejectReason::InvalidConfig, n.max(1)),
                        Ok(policy) => {
                            for mut e in normalized.events {
                                if policy.admit(&e).is_err() {
                                    rec.excluded = rec.excluded.saturating_add(1);
                                    continue;
                                }
                                policy.sanitize(&mut e);
                                if e.validate().is_err() {
                                    reject(&mut rec, RejectReason::InvalidEvent, 1);
                                    continue;
                                }
                                rec.events.push(e);
                            }
                        }
                    }
                }
            },
        },
    }
    report.events = rec.events.len();
    report.excluded = rec.excluded;
    report.outcome = spool.write(&rec).map_err(|e| e.to_string());
    if report.outcome.is_err() {
        // Nowhere to write the record: count it if the counter works.
        let _counted_if_possible = spool.add_overflow(1, ctx.received_at);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spool::Spool;

    fn setup() -> (tempfile::TempDir, Spool, CapturePolicy) {
        let t = tempfile::tempdir().unwrap();
        let s = Spool::at(t.path().join("spool"));
        let p = CapturePolicy::from_config(&PrivacyConfig::default()).unwrap();
        (t, s, p)
    }

    fn spooled(s: &Spool) -> Vec<SpoolRecord> {
        s.pending(100)
            .unwrap()
            .iter()
            .map(|p| Spool::read(p).unwrap())
            .collect()
    }

    #[test]
    fn secrets_never_reach_the_spool() {
        let (_t, s, p) = setup();
        let input = serde_json::json!({
            "hook_event_name":"PreToolUse","session_id":"s","cwd":"/w","tool_name":"Bash","tool_use_id":"t",
            "tool_input":{"command":"curl -H 'Authorization: Bearer sk-ant-api03-SECRETSECRETSECRETSECRET' https://x"}
        })
        .to_string();
        let r = run(AgentKind::ClaudeCode, &mut input.as_bytes(), Ok(&p), &s);
        assert_eq!(r.events, 2);
        let raw = std::fs::read_to_string(&s.pending(1).unwrap()[0]).unwrap();
        assert!(
            !raw.contains("SECRET"),
            "metadata mode drops the command payload"
        );
        assert!(raw.contains("payload_dropped"));
    }

    #[test]
    fn redacted_mode_keeps_a_redacted_command() {
        let (_t, s, _) = setup();
        let p = CapturePolicy::from_config(&PrivacyConfig {
            capture: crate::privacy::CaptureMode::Redacted,
            ..PrivacyConfig::default()
        })
        .unwrap();
        let input = r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"Bash","tool_use_id":"t",
            "tool_input":{"command":"psql postgres://app:hunter22@db/app"}}"#;
        run(AgentKind::ClaudeCode, &mut input.as_bytes(), Ok(&p), &s);
        let recs = spooled(&s);
        let cmd = recs[0]
            .events
            .iter()
            .find(|e| e.event_type == "command.started")
            .unwrap();
        let payload = cmd.payload.as_deref().unwrap();
        assert!(
            payload.contains("psql") && !payload.contains("hunter22"),
            "{payload}"
        );
    }

    #[test]
    fn excluded_paths_are_counted_not_written() {
        let (_t, s, _) = setup();
        let p = CapturePolicy::from_config(&PrivacyConfig {
            allowed_project_dirs: vec!["/work".into()],
            excluded_file_globs: vec!["**/.env".into()],
            ..PrivacyConfig::default()
        })
        .unwrap();
        let outside = r#"{"hook_event_name":"PostToolUse","session_id":"s","cwd":"/home/me/private","tool_name":"Read","tool_use_id":"t","tool_input":{"file_path":"/home/me/private/x"}}"#;
        let r = run(AgentKind::ClaudeCode, &mut outside.as_bytes(), Ok(&p), &s);
        assert_eq!((r.events, r.excluded), (0, 2));
        let env = r#"{"hook_event_name":"PostToolUse","session_id":"s","cwd":"/work/app","tool_name":"Read","tool_use_id":"u","tool_input":{"file_path":".env"}}"#;
        let r = run(AgentKind::ClaudeCode, &mut env.as_bytes(), Ok(&p), &s);
        assert_eq!(
            (r.events, r.excluded),
            (1, 1),
            "the tool call is kept, the .env read is not"
        );
        let traversal = r#"{"hook_event_name":"PostToolUse","session_id":"s","cwd":"/work/app","tool_name":"Read","tool_use_id":"v","tool_input":{"file_path":"../../home/me/.ssh/id_rsa"}}"#;
        let r = run(AgentKind::ClaudeCode, &mut traversal.as_bytes(), Ok(&p), &s);
        assert_eq!(r.excluded, 1, "`..` is resolved before the allowlist check");
    }

    #[test]
    fn malformed_oversized_and_config_errors_are_recorded_loss() {
        let (_t, s, p) = setup();
        let r = run(AgentKind::Codex, &mut "{nope".as_bytes(), Ok(&p), &s);
        assert_eq!(r.rejected, [RejectReason::MalformedInput]);
        let big = vec![b' '; MAX_HOOK_INPUT_BYTES + 10];
        let r = run(AgentKind::Codex, &mut big.as_slice(), Ok(&p), &s);
        assert_eq!(r.rejected, [RejectReason::OversizedInput]);
        let ok = r#"{"hook_event_name":"Stop","session_id":"s","turn_id":"t"}"#;
        let r = run(AgentKind::Codex, &mut ok.as_bytes(), Err("bad config"), &s);
        assert_eq!(r.rejected, [RejectReason::InvalidConfig]);
        let recs = spooled(&s);
        assert_eq!(recs.len(), 3);
        assert!(recs
            .iter()
            .all(|r| r.events.is_empty() && !r.rejected.is_empty()));
    }

    #[test]
    fn ignored_events_write_nothing() {
        let (_t, s, p) = setup();
        let r = run(
            AgentKind::ClaudeCode,
            &mut r#"{"hook_event_name":"Notification","session_id":"s"}"#.as_bytes(),
            Ok(&p),
            &s,
        );
        assert!(r.ignored);
        assert_eq!(r.outcome, Ok(WriteOutcome::Empty));
        assert!(s.pending(10).unwrap().is_empty());
    }

    #[test]
    fn policy_config_loads_and_rejects_typos() {
        let t = tempfile::tempdir().unwrap();
        assert!(load_policy(t.path()).is_ok(), "missing file = defaults");
        std::fs::write(config_path(t.path()), "[privacy]\ncapture = \"redacted\"\n").unwrap();
        assert_eq!(
            load_policy(t.path()).unwrap().mode,
            crate::privacy::CaptureMode::Redacted
        );
        std::fs::write(config_path(t.path()), "[privacy]\ncaptur = \"full\"\n").unwrap();
        assert!(load_policy(t.path()).is_err());
    }
}
