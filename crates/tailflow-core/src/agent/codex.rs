//! OpenAI Codex adapter.
//!
//! Input: the JSON object Codex writes to a command hook's stdin. Hooks are
//! stable since Codex 0.124 and enabled by default; field names follow the
//! published input schemas (`codex-rs/hooks/schema/generated/*.input.schema.json`
//! at `rust-v0.172.0`): common `session_id`, `cwd`, `hook_event_name`, `model`,
//! `permission_mode`; `turn_id` on turn-scoped events; `tool_name`,
//! `tool_use_id`, `tool_input` on tool events, plus `tool_response` on
//! `PostToolUse`; `agent_id`/`agent_type` inside subagents (which share the
//! parent's `session_id`).
//!
//! What Codex does **not** report, and TailFlow therefore does not claim:
//! - No failure hook: `PostToolUse` runs after a Bash command whatever its
//!   exit status, and `tool_response` has no documented exit code. A command
//!   end event (`command.completed` / `command.failed`) is emitted only when
//!   the response carries a numeric `exit_code`; otherwise the tool event says
//!   `command.outcome = unknown`.
//! - Hosted tools (web search) are not hooked.
//! - File changes are known only from `apply_patch` (its patch headers);
//!   shell edits are invisible.
//!
//! Token usage *is* reported, per model attempt, by `PostModelRequest`
//! (Codex 0.171+). TailFlow records counts only, never a cost estimate.

use super::{
    correlation, display_relative, normalize_path, put, safe_source_id, safe_token, session,
    summarize_command, AdapterError, AgentKind, Capability, CoverageLevel, EventBuilder,
    HookContext, Normalized,
};
use crate::event::EventStatus;
use crate::LogLevel;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Hook events and the Codex version that introduced each (release notes).
/// The installer registers only events the installed version supports.
pub const HOOK_EVENTS: &[(&str, (u32, u32, u32))] = &[
    ("SessionStart", (0, 124, 0)),
    ("UserPromptSubmit", (0, 124, 0)),
    ("PreToolUse", (0, 124, 0)),
    ("PostToolUse", (0, 124, 0)),
    ("Stop", (0, 124, 0)),
    ("SubagentStart", (0, 133, 0)),
    ("SubagentStop", (0, 133, 0)),
    ("SessionEnd", (0, 145, 0)),
    ("PostModelRequest", (0, 171, 0)),
];

/// Lowest Codex version with stable hooks.
pub const MIN_VERSION: (u32, u32, u32) = (0, 124, 0);

pub const CAPABILITIES: &[Capability] = &[
    cap("session.lifecycle", CoverageLevel::Direct, "SessionStart; SessionEnd (0.145+) may arrive late and has reason \"other\" only"),
    cap("turns", CoverageLevel::Direct, "UserPromptSubmit (metadata only) and Stop, keyed by turn_id"),
    cap("tool.calls", CoverageLevel::Partial, "PreToolUse / PostToolUse for Bash, apply_patch, MCP and local function tools; hosted tools (web search) are not hooked; no failure event"),
    cap("tool.duration", CoverageLevel::None, "not in hook input"),
    cap("commands", CoverageLevel::Partial, "Bash tool calls; exit status only when tool_response carries exit_code (not documented)"),
    cap("file.read", CoverageLevel::None, "Codex has no read tool hook; reads happen through shell commands"),
    cap("file.write", CoverageLevel::Partial, "apply_patch Add/Update/Delete/Move headers; shell edits are invisible; nothing is verified on disk"),
    cap("subagents", CoverageLevel::Direct, "SubagentStart / SubagentStop (0.133+) with agent_id"),
    cap("tokens.cost", CoverageLevel::Partial, "token counts per model attempt from PostModelRequest (0.171+); no cost"),
    cap("agent.version", CoverageLevel::None, "not in hook input"),
];

const fn cap(activity: &'static str, level: CoverageLevel, note: &'static str) -> Capability {
    Capability {
        activity,
        level,
        note,
    }
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "shell" | "local_shell" | "exec_command" | "unified_exec" | "container.exec"
    )
}

/// A file change named in an `apply_patch` envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchChange {
    pub event_type: &'static str,
    pub path: String,
    /// For a move: the source path.
    pub from: Option<String>,
}

/// Parse the headers of a Codex `apply_patch` envelope. Hunk bodies (the
/// file contents) are never looked at beyond the header lines. At most
/// `limit` changes are returned.
pub fn parse_patch(patch: &str, limit: usize) -> Vec<PatchChange> {
    let mut out: Vec<PatchChange> = Vec::new();
    for line in patch.lines() {
        if out.len() >= limit {
            break;
        }
        let line = line.trim_end();
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            out.push(PatchChange {
                event_type: "file.created",
                path: p.trim().into(),
                from: None,
            });
        } else if let Some(p) = line.strip_prefix("*** Delete File: ") {
            out.push(PatchChange {
                event_type: "file.deleted",
                path: p.trim().into(),
                from: None,
            });
        } else if let Some(p) = line.strip_prefix("*** Update File: ") {
            out.push(PatchChange {
                event_type: "file.modified",
                path: p.trim().into(),
                from: None,
            });
        } else if let Some(p) = line.strip_prefix("*** Move to: ") {
            // Follows the Update header of the file being moved.
            if let Some(last) = out.last_mut() {
                if last.event_type == "file.modified" && last.from.is_none() {
                    last.from = Some(std::mem::replace(&mut last.path, p.trim().into()));
                    last.event_type = "file.renamed";
                }
            }
        }
    }
    out.retain(|c| !c.path.is_empty());
    out
}

/// Most file events recorded for one patch; the rest are counted.
const MAX_PATCH_FILES: usize = 32;

/// Normalize one Codex hook input.
///
/// # Errors
/// Not a JSON object, or no `hook_event_name`.
pub fn normalize(input: &Value, ctx: &HookContext) -> Result<Normalized, AdapterError> {
    let obj = input
        .as_object()
        .ok_or_else(|| AdapterError::Malformed("expected a JSON object".into()))?;
    let hook = safe_token(obj.get("hook_event_name"), 64)
        .ok_or(AdapterError::MissingField("hook_event_name"))?;

    let mut b = EventBuilder::new(AgentKind::Codex, ctx, &hook);
    b.session_key = obj
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|s| session::session_key(AgentKind::Codex, s));
    let cwd: Option<PathBuf> = obj
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(|c| normalize_path(c, None));
    b.cwd.clone_from(&cwd);
    let at_boundary = matches!(hook.as_str(), "SessionStart" | "SessionEnd");
    let git = if at_boundary {
        cwd.as_deref().and_then(correlation::discover)
    } else {
        None
    };
    let project_root: Option<PathBuf> = git
        .as_ref()
        .map(|g| g.worktree_root.clone())
        .or_else(|| cwd.clone());
    b.project_id = project_root.as_ref().map(|p| p.display().to_string());

    let turn = safe_source_id(obj.get("turn_id")).map(str::to_string);
    let subagent = safe_source_id(obj.get("agent_id")).map(str::to_string);
    let subagent_type = safe_token(obj.get("agent_type"), 128);
    let model = safe_token(obj.get("model"), 128);

    let mut out = Normalized::default();
    match hook.as_str() {
        "SessionStart" => {
            let mut e = b.event("agent.session.started", None);
            let source = safe_token(obj.get("source"), 32);
            e.message = Some(format!(
                "Codex session started ({})",
                source.as_deref().unwrap_or("unknown source")
            ));
            put(&mut e.attributes, "session.source", source);
            put(&mut e.attributes, "session.model", model);
            put(
                &mut e.attributes,
                "session.permission_mode",
                safe_token(obj.get("permission_mode"), 32),
            );
            correlation::add_git_attributes(&mut e, git.as_ref());
            out.events.push(e);
        }
        "SessionEnd" => {
            let mut e = b.event("agent.session.ended", None);
            let reason = safe_token(obj.get("reason"), 64);
            e.message = Some(format!(
                "Codex session ended ({})",
                reason.as_deref().unwrap_or("no reason given")
            ));
            put(&mut e.attributes, "session.end_reason", reason);
            correlation::add_git_attributes(&mut e, git.as_ref());
            out.events.push(e);
        }
        "UserPromptSubmit" => {
            let mut e = b.event(
                "agent.prompt.submitted",
                turn.as_ref().map(|t| format!("prompt:{t}")),
            );
            e.message = Some("prompt submitted (text not recorded)".into());
            let chars = obj
                .get("prompt")
                .and_then(Value::as_str)
                .map(|p| p.chars().count());
            put(&mut e.attributes, "prompt.chars", chars);
            put(&mut e.attributes, "turn.id", turn.clone());
            out.events.push(e);
        }
        "Stop" => {
            let mut e = b.event(
                "agent.turn.completed",
                turn.as_ref().map(|t| format!("turn:{t}:stop")),
            );
            e.status = Some(EventStatus::Succeeded);
            e.operation_id = turn.as_ref().map(|t| format!("turn:{t}"));
            e.message = Some("turn completed".into());
            put(&mut e.attributes, "turn.id", turn.clone());
            out.events.push(e);
        }
        "SubagentStart" | "SubagentStop" => {
            let stop = hook == "SubagentStop";
            let mut e = b.event(
                if stop {
                    "agent.subagent.stopped"
                } else {
                    "agent.subagent.started"
                },
                subagent
                    .as_ref()
                    .map(|a| format!("subagent:{a}:{}", if stop { "stop" } else { "start" })),
            );
            e.operation_id = subagent.as_ref().map(|a| format!("subagent:{a}"));
            e.parent_operation_id = turn.as_ref().map(|t| format!("turn:{t}"));
            e.message = Some(format!(
                "subagent {} ({})",
                if stop { "stopped" } else { "started" },
                subagent_type.as_deref().unwrap_or("unknown type")
            ));
            put(&mut e.attributes, "agent.subagent_id", subagent.clone());
            put(&mut e.attributes, "agent.type", subagent_type);
            out.events.push(e);
        }
        "PostModelRequest" => {
            model_request(
                &mut b,
                obj,
                turn.as_deref(),
                subagent.as_deref(),
                model,
                &mut out,
            );
        }
        "PreToolUse" | "PostToolUse" => {
            tool_events(
                &mut b,
                obj,
                hook == "PostToolUse",
                turn.as_deref(),
                subagent.as_deref(),
                project_root.as_deref(),
                &mut out,
            );
        }
        other => out.ignored = Some(format!("hook event {other} is not recorded")),
    }
    Ok(out)
}

fn model_request(
    b: &mut EventBuilder<'_>,
    obj: &Map<String, Value>,
    turn: Option<&str>,
    subagent: Option<&str>,
    model: Option<String>,
    out: &mut Normalized,
) {
    let request = safe_source_id(obj.get("request_id"));
    let attempt = obj.get("attempt_number").and_then(Value::as_u64);
    let status = obj.get("status").and_then(Value::as_str);
    let derived = request.map(|r| format!("model:{r}:{}", attempt.unwrap_or(0)));
    let mut e = b.event("agent.model.completed", derived);
    e.parent_operation_id = subagent
        .map(|a| format!("subagent:{a}"))
        .or_else(|| turn.map(|t| format!("turn:{t}")));
    let (st, sev) = match status {
        Some("completed") => (EventStatus::Succeeded, LogLevel::Info),
        Some("cancelled") => (EventStatus::Cancelled, LogLevel::Warn),
        _ => (EventStatus::Failed, LogLevel::Error),
    };
    e.status = Some(st);
    e.severity = sev;
    put(&mut e.attributes, "model.name", model.clone());
    put(
        &mut e.attributes,
        "model.kind",
        safe_token(obj.get("kind"), 32),
    );
    put(&mut e.attributes, "model.attempt", attempt);
    put(
        &mut e.attributes,
        "model.request_id",
        request.map(str::to_string),
    );
    put(&mut e.attributes, "turn.id", turn.map(str::to_string));
    let mut total = None;
    if let Some(usage) = obj.get("token_usage").and_then(Value::as_object) {
        for (src, dst) in [
            ("input_tokens", "tokens.input"),
            ("cached_input_tokens", "tokens.cached_input"),
            ("cache_write_input_tokens", "tokens.cache_write_input"),
            ("output_tokens", "tokens.output"),
            ("reasoning_output_tokens", "tokens.reasoning_output"),
            ("total_tokens", "tokens.total"),
        ] {
            let v = usage.get(src).and_then(Value::as_u64);
            if src == "total_tokens" {
                total = v;
            }
            put(&mut e.attributes, dst, v);
        }
    } else {
        e.attributes
            .insert("tokens.reported".into(), Value::Bool(false));
    }
    if let Some(err) = obj.get("error").and_then(Value::as_str) {
        e.payload = Some(err.to_string());
    }
    e.message = Some(match total {
        Some(t) => format!(
            "model request {} ({t} tokens)",
            status.unwrap_or("finished")
        ),
        None => format!("model request {}", status.unwrap_or("finished")),
    });
    out.events.push(e);
}

/// The command line from `tool_input.command`: a string, or an argv array.
fn command_text(input: &Value) -> Option<String> {
    match input.get("command")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// A numeric exit code, if the response carries one at a known key.
fn response_exit_code(response: Option<&Value>) -> Option<i64> {
    let r = response?;
    r.get("exit_code")
        .or_else(|| r.get("metadata").and_then(|m| m.get("exit_code")))
        .and_then(Value::as_i64)
}

fn tool_events(
    b: &mut EventBuilder<'_>,
    obj: &Map<String, Value>,
    post: bool,
    turn: Option<&str>,
    subagent: Option<&str>,
    project_root: Option<&Path>,
    out: &mut Normalized,
) {
    let tool = safe_token(obj.get("tool_name"), 128).unwrap_or_else(|| "unknown".into());
    let use_id = safe_source_id(obj.get("tool_use_id")).map(str::to_string);
    let input = obj.get("tool_input").cloned().unwrap_or(Value::Null);
    let response = obj.get("tool_response");
    let phase = if post { "post" } else { "pre" };
    let tool_op = use_id.as_ref().map(|u| format!("tool:{u}"));
    let parent = subagent
        .map(|a| format!("subagent:{a}"))
        .or_else(|| turn.map(|t| format!("turn:{t}")));
    let shell = is_shell_tool(&tool);
    let exit = if post && shell {
        response_exit_code(response)
    } else {
        None
    };

    let mut e = b.event(
        if post {
            "agent.tool.completed"
        } else {
            "agent.tool.started"
        },
        use_id.as_ref().map(|u| format!("tool:{u}:{phase}")),
    );
    e.operation_id.clone_from(&tool_op);
    e.parent_operation_id.clone_from(&parent);
    // "completed" = the call returned output; Codex reports no tool failure.
    e.status = Some(if post {
        EventStatus::Succeeded
    } else {
        EventStatus::Started
    });
    e.message = Some(format!(
        "{tool} {}",
        if post { "returned" } else { "started" }
    ));
    put(&mut e.attributes, "tool.name", Some(tool.clone()));
    put(&mut e.attributes, "tool.use_id", use_id.clone());
    put(&mut e.attributes, "turn.id", turn.map(str::to_string));
    put(
        &mut e.attributes,
        "agent.subagent_id",
        subagent.map(str::to_string),
    );
    if let Some(server) = tool
        .strip_prefix("mcp__")
        .and_then(|t| t.split("__").next())
    {
        put(
            &mut e.attributes,
            "tool.mcp_server",
            Some(server.to_string()),
        );
    }
    if post && shell && exit.is_none() {
        put(&mut e.attributes, "command.outcome", Some("unknown"));
    }
    out.events.push(e);

    if shell {
        let cmd = command_text(&input).unwrap_or_default();
        let summary = summarize_command(&cmd);
        let program = summary.program.clone().unwrap_or_else(|| "?".into());
        let end = match (post, exit) {
            (false, _) => Some(("command.started", EventStatus::Started, LogLevel::Info)),
            (true, Some(0)) => Some(("command.completed", EventStatus::Succeeded, LogLevel::Info)),
            (true, Some(_)) => Some(("command.failed", EventStatus::Failed, LogLevel::Error)),
            (true, None) => None,
        };
        if let Some((cty, st, sev)) = end {
            let mut c = b.event(cty, use_id.as_ref().map(|u| format!("cmd:{u}:{phase}")));
            c.operation_id = use_id.as_ref().map(|u| format!("cmd:{u}"));
            c.parent_operation_id.clone_from(&tool_op);
            c.status = Some(st);
            c.severity = sev;
            c.message = Some(match (st, exit) {
                (EventStatus::Started, _) => format!("command `{program}` started"),
                (EventStatus::Succeeded, _) => format!("command `{program}` completed"),
                (_, Some(code)) => format!("command `{program}` failed (exit {code})"),
                _ => format!("command `{program}` failed"),
            });
            put(&mut c.attributes, "command.program", summary.program);
            c.attributes
                .insert("command.compound".into(), Value::Bool(summary.compound));
            put(&mut c.attributes, "command.chars", Some(summary.chars));
            put(&mut c.attributes, "tool.name", Some(tool.clone()));
            put(&mut c.attributes, "tool.use_id", use_id.clone());
            if let Some(code) = exit {
                put(&mut c.attributes, "command.exit_code", Some(code));
                put(
                    &mut c.attributes,
                    "command.exit_code_source",
                    Some("tool_response"),
                );
            }
            if !cmd.is_empty() {
                c.payload = Some(cmd);
            }
            out.events.push(c);
        }
    }

    if post && tool == "apply_patch" {
        let patch = command_text(&input).unwrap_or_default();
        let changes = parse_patch(&patch, MAX_PATCH_FILES + 1);
        let more = changes.len() > MAX_PATCH_FILES;
        for (i, ch) in changes.iter().take(MAX_PATCH_FILES).enumerate() {
            let Some(path) = normalize_path(&ch.path, b.cwd.as_deref()) else {
                continue;
            };
            let mut f = b.event(
                ch.event_type,
                use_id.as_ref().map(|u| format!("file:{u}:{i}")),
            );
            f.parent_operation_id.clone_from(&tool_op);
            f.path = Some(path.display().to_string());
            let verb = ch.event_type.trim_start_matches("file.");
            f.message = Some(format!(
                "{verb} {} (reported by apply_patch)",
                display_relative(&path, project_root)
            ));
            if let Some(from) = ch
                .from
                .as_deref()
                .and_then(|p| normalize_path(p, b.cwd.as_deref()))
            {
                put(
                    &mut f.attributes,
                    "file.renamed_from",
                    Some(from.display().to_string()),
                );
            }
            put(&mut f.attributes, "tool.name", Some(tool.clone()));
            put(&mut f.attributes, "tool.use_id", use_id.clone());
            put(&mut f.attributes, "attribution", Some("agent_reported"));
            f.attributes
                .insert("verified_on_disk".into(), Value::Bool(false));
            if more && i + 1 == MAX_PATCH_FILES {
                f.attributes
                    .insert("file.patch_truncated".into(), Value::Bool(true));
            }
            out.events.push(f);
        }
    }
}

/// Parse `codex --version` output (`codex-cli 0.172.0`).
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let v = text
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut it = v.split(['.', '-', '+']).map(str::parse::<u32>);
    Some((
        it.next()?.ok()?,
        it.next()?.ok()?,
        it.next().and_then(Result::ok).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> HookContext {
        HookContext {
            received_at: chrono::Utc::now(),
            nonce: "n".into(),
        }
    }
    fn run(v: Value) -> Normalized {
        normalize(&v, &ctx()).unwrap()
    }
    fn types(n: &Normalized) -> Vec<&str> {
        n.events.iter().map(|e| e.event_type.as_str()).collect()
    }

    #[test]
    fn bash_without_exit_code_claims_no_outcome() {
        let pre = run(
            json!({"hook_event_name":"PreToolUse","session_id":"s1","turn_id":"t1","cwd":"/w",
            "tool_name":"Bash","tool_use_id":"call_1","tool_input":{"command":"npm test"},"model":"gpt-5.5"}),
        );
        assert_eq!(types(&pre), ["agent.tool.started", "command.started"]);
        let post = run(
            json!({"hook_event_name":"PostToolUse","session_id":"s1","turn_id":"t1","cwd":"/w",
            "tool_name":"Bash","tool_use_id":"call_1","tool_input":{"command":"npm test"},
            "tool_response":"some output"}),
        );
        assert_eq!(types(&post), ["agent.tool.completed"]);
        assert_eq!(post.events[0].attributes["command.outcome"], "unknown");
        let with_code = run(
            json!({"hook_event_name":"PostToolUse","session_id":"s1","cwd":"/w",
            "tool_name":"Bash","tool_use_id":"call_2","tool_input":{"command":["cargo","build"]},
            "tool_response":{"exit_code":1}}),
        );
        assert_eq!(
            types(&with_code),
            ["agent.tool.completed", "command.failed"]
        );
        assert_eq!(with_code.events[1].attributes["command.program"], "cargo");
        for e in pre
            .events
            .iter()
            .chain(&post.events)
            .chain(&with_code.events)
        {
            e.validate().unwrap();
            assert_eq!(e.agent_name.as_deref(), Some("codex"));
        }
    }

    #[test]
    fn apply_patch_reports_each_file() {
        let patch = "*** Begin Patch\n*** Add File: src/new.rs\n+fn x() {}\n*** Update File: src/old.rs\n*** Move to: src/moved.rs\n@@\n-a\n+b\n*** Delete File: gone.txt\n*** End Patch";
        let n = run(
            json!({"hook_event_name":"PostToolUse","session_id":"s","cwd":"/w/p",
            "tool_name":"apply_patch","tool_use_id":"c","tool_input":{"command":patch},"tool_response":{}}),
        );
        assert_eq!(
            types(&n),
            [
                "agent.tool.completed",
                "file.created",
                "file.renamed",
                "file.deleted"
            ]
        );
        assert_eq!(n.events[2].path.as_deref(), Some("/w/p/src/moved.rs"));
        assert_eq!(
            n.events[2].attributes["file.renamed_from"],
            "/w/p/src/old.rs"
        );
        assert!(!serde_json::to_string(&n.events).unwrap().contains("fn x()"));
        let pre = run(
            json!({"hook_event_name":"PreToolUse","session_id":"s","cwd":"/w",
            "tool_name":"apply_patch","tool_use_id":"c","tool_input":{"command":patch}}),
        );
        assert_eq!(
            types(&pre),
            ["agent.tool.started"],
            "no file claim before it ran"
        );
    }

    #[test]
    fn patch_file_count_is_bounded() {
        let mut p = String::new();
        for i in 0..100 {
            p.push_str(&format!("*** Add File: f{i}\n"));
        }
        let n = run(
            json!({"hook_event_name":"PostToolUse","session_id":"s","cwd":"/w",
            "tool_name":"apply_patch","tool_use_id":"c","tool_input":{"command":p}}),
        );
        assert_eq!(n.events.len(), 1 + MAX_PATCH_FILES);
        assert_eq!(
            n.events.last().unwrap().attributes["file.patch_truncated"],
            true
        );
    }

    #[test]
    fn model_requests_record_tokens_not_cost() {
        let n = run(
            json!({"hook_event_name":"PostModelRequest","session_id":"s","turn_id":"t","cwd":"/w",
            "model":"gpt-5.5","request_id":"r1","attempt_number":1,"kind":"sampling","thread_id":"th",
            "status":"completed","error":null,"response_id":"x",
            "token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7,
                           "reasoning_output_tokens":3,"total_tokens":107}}),
        );
        let e = &n.events[0];
        assert_eq!(e.event_type, "agent.model.completed");
        assert_eq!(e.attributes["tokens.total"], 107);
        assert!(!e
            .attributes
            .keys()
            .any(|k| k.contains("cost") || k.contains("usd")));
        e.validate().unwrap();
    }

    #[test]
    fn turns_subagents_and_sessions() {
        let s = run(
            json!({"hook_event_name":"SessionStart","session_id":"s","cwd":"/w","source":"startup","model":"m"}),
        );
        assert_eq!(types(&s), ["agent.session.started"]);
        let t = run(
            json!({"hook_event_name":"Stop","session_id":"s","turn_id":"t9","last_assistant_message":"private"}),
        );
        assert_eq!(t.events[0].operation_id.as_deref(), Some("turn:t9"));
        assert!(!serde_json::to_string(&t.events)
            .unwrap()
            .contains("private"));
        let sa = run(
            json!({"hook_event_name":"SubagentStart","session_id":"s","turn_id":"t9","agent_id":"ag","agent_type":"worker"}),
        );
        assert_eq!(sa.events[0].parent_operation_id.as_deref(), Some("turn:t9"));
        let e = run(json!({"hook_event_name":"SessionEnd","session_id":"s","reason":"other"}));
        assert_eq!(e.events[0].session_id.as_deref(), Some("codex:s"));
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("codex-cli 0.172.0"), Some((0, 172, 0)));
        assert_eq!(
            parse_version("codex-cli 0.124.0-alpha.2\n"),
            Some((0, 124, 0))
        );
        assert_eq!(parse_version("garbage"), None);
    }
}
