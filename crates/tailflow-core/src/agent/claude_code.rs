//! Claude Code adapter.
//!
//! Input: the JSON object Claude Code writes to a command hook's stdin
//! (<https://code.claude.com/docs/en/hooks>). Verified against Claude Code
//! 2.1.331's settings schema and binary: common fields `session_id`, `cwd`,
//! `hook_event_name`, `permission_mode`, `prompt_id`, `agent_id`/`agent_type`
//! (inside subagents); `tool_use_id` on `PreToolUse`, `PostToolUse` and
//! `PostToolUseFailure`; `duration_ms` on the two post events.
//!
//! Version tolerance: every field except `hook_event_name` is optional and
//! unknown fields are ignored. Without `tool_use_id` (older versions) start
//! and end events are still recorded but cannot be paired; without
//! `session_id` events are recorded with no session. Unknown hook events
//! produce nothing (reported as ignored), never an error.
//!
//! Never recorded: prompt text, assistant messages, tool output, file
//! contents, `transcript_path` contents. Shell command text is placed in the
//! event *payload*, which the default `metadata` capture mode drops.

use super::{
    correlation, display_relative, normalize_path, put, safe_source_id, safe_token, session,
    summarize_command, AdapterError, AgentKind, Capability, CoverageLevel, EventBuilder,
    HookContext, Normalized,
};
use crate::event::EventStatus;
use crate::LogLevel;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Hook events `tailflow agent install claude` registers, in install order.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
];

/// What the Claude Code integration can see.
pub const CAPABILITIES: &[Capability] = &[
    cap("session.lifecycle", CoverageLevel::Direct, "SessionStart (startup/resume/clear/compact/fork) and SessionEnd with reason"),
    cap("turns", CoverageLevel::Direct, "UserPromptSubmit (metadata only) and Stop / StopFailure"),
    cap("tool.calls", CoverageLevel::Direct, "PreToolUse / PostToolUse / PostToolUseFailure, paired by tool_use_id; every tool, including MCP tools"),
    cap("tool.duration", CoverageLevel::Direct, "duration_ms on PostToolUse / PostToolUseFailure"),
    cap("commands", CoverageLevel::Partial, "Bash/PowerShell tool calls only; exit code only on failure (parsed from the error text); commands run inside scripts are invisible"),
    cap("file.read", CoverageLevel::Partial, "Read tool only; reads by shell commands are invisible"),
    cap("file.write", CoverageLevel::Partial, "Write/Edit/MultiEdit/NotebookEdit tool reports; shell redirections, mv, rm are invisible; nothing is verified on disk"),
    cap("subagents", CoverageLevel::Direct, "SubagentStart / SubagentStop with agent_id; tool calls inside carry agent_id"),
    cap("tokens.cost", CoverageLevel::None, "not in hook input (transcripts are an internal format and are not read)"),
    cap("agent.version", CoverageLevel::None, "not in hook input"),
];

const fn cap(activity: &'static str, level: CoverageLevel, note: &'static str) -> Capability {
    Capability {
        activity,
        level,
        note,
    }
}

/// Tools whose `tool_input.command` is a shell command line.
fn is_shell_tool(name: &str) -> bool {
    matches!(name, "Bash" | "PowerShell")
}

/// The file a file tool acts on and the event a successful call implies.
fn file_effect<'a>(
    tool: &str,
    input: &'a Value,
    response: Option<&Value>,
) -> Option<(&'a str, &'static str, Option<&'static str>)> {
    let path = |k: &str| input.get(k).and_then(Value::as_str);
    match tool {
        "Read" => Some((path("file_path")?, "file.read", None)),
        "Write" => {
            let p = path("file_path")?;
            match response.and_then(|r| r.get("type")).and_then(Value::as_str) {
                Some("create") => Some((p, "file.created", None)),
                Some("update") => Some((p, "file.modified", None)),
                // No (or an unknown) response type: the tool wrote the file,
                // whether it existed before is not reported.
                _ => Some((p, "file.modified", Some("create_or_update"))),
            }
        }
        "Edit" | "MultiEdit" => Some((path("file_path")?, "file.modified", None)),
        "NotebookEdit" => Some((path("notebook_path")?, "file.modified", None)),
        _ => None,
    }
}

/// `"Exit code 2\n…"` → 2. Claude Code reports a failed shell command's exit
/// status only inside the error text.
fn exit_code_from_error(err: &str) -> Option<i64> {
    let rest = err.trim_start().strip_prefix("Exit code ")?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

/// Normalize one Claude Code hook input.
///
/// # Errors
/// Not a JSON object, or no `hook_event_name`.
pub fn normalize(input: &Value, ctx: &HookContext) -> Result<Normalized, AdapterError> {
    let obj = input
        .as_object()
        .ok_or_else(|| AdapterError::Malformed("expected a JSON object".into()))?;
    let hook = obj
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or(AdapterError::MissingField("hook_event_name"))?;
    let hook = safe_token(Some(&Value::from(hook)), 64)
        .ok_or(AdapterError::MissingField("hook_event_name"))?;

    let mut b = EventBuilder::new(AgentKind::ClaudeCode, ctx, &hook);
    b.session_key = obj
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|s| session::session_key(AgentKind::ClaudeCode, s));
    let cwd: Option<PathBuf> = obj
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(|c| normalize_path(c, None));
    b.cwd.clone_from(&cwd);

    // Git discovery only at session boundaries: a few small file reads, never
    // per tool call.
    let at_boundary = matches!(hook.as_str(), "SessionStart" | "SessionEnd");
    let git = if at_boundary {
        cwd.as_deref().and_then(correlation::discover)
    } else {
        None
    };
    let project_root: Option<PathBuf> = git
        .as_ref()
        .map(|g| g.worktree_root.clone())
        .or_else(|| cwd.as_deref().and_then(project_root_hint));
    b.project_id = project_root.as_ref().map(|p| p.display().to_string());

    let subagent = safe_source_id(obj.get("agent_id")).map(str::to_string);
    let subagent_type = safe_token(obj.get("agent_type"), 128);

    let mut out = Normalized::default();
    match hook.as_str() {
        "SessionStart" => {
            let mut e = b.event("agent.session.started", None);
            let source = safe_token(obj.get("source"), 32);
            e.message = Some(format!(
                "Claude Code session started ({})",
                source.as_deref().unwrap_or("unknown source")
            ));
            put(&mut e.attributes, "session.source", source);
            put(
                &mut e.attributes,
                "session.model",
                safe_token(obj.get("model"), 128),
            );
            put(
                &mut e.attributes,
                "session.permission_mode",
                safe_token(obj.get("permission_mode"), 32),
            );
            put(&mut e.attributes, "agent.type", subagent_type);
            correlation::add_git_attributes(&mut e, git.as_ref());
            out.events.push(e);
        }
        "SessionEnd" => {
            let mut e = b.event("agent.session.ended", None);
            let reason = safe_token(obj.get("reason"), 64);
            e.message = Some(format!(
                "Claude Code session ended ({})",
                reason.as_deref().unwrap_or("no reason given")
            ));
            put(&mut e.attributes, "session.end_reason", reason);
            correlation::add_git_attributes(&mut e, git.as_ref());
            out.events.push(e);
        }
        "UserPromptSubmit" => {
            let derived = safe_source_id(obj.get("prompt_id")).map(|p| format!("prompt:{p}"));
            let mut e = b.event("agent.prompt.submitted", derived);
            e.message = Some("prompt submitted (text not recorded)".into());
            let chars = obj
                .get("prompt")
                .and_then(Value::as_str)
                .map(|p| p.chars().count());
            put(&mut e.attributes, "prompt.chars", chars);
            put(
                &mut e.attributes,
                "prompt.id",
                safe_source_id(obj.get("prompt_id")),
            );
            out.events.push(e);
        }
        "Stop" | "StopFailure" => {
            let failed = hook == "StopFailure";
            let derived = safe_source_id(obj.get("prompt_id"))
                .map(|p| format!("turn:{p}:{}", if failed { "failed" } else { "stop" }));
            let mut e = b.event(
                if failed {
                    "agent.turn.failed"
                } else {
                    "agent.turn.completed"
                },
                derived,
            );
            e.status = Some(if failed {
                EventStatus::Failed
            } else {
                EventStatus::Succeeded
            });
            if failed {
                e.severity = LogLevel::Error;
                let kind = safe_token(obj.get("error"), 64);
                e.message = Some(format!(
                    "turn failed ({})",
                    kind.as_deref().unwrap_or("unknown error")
                ));
                put(&mut e.attributes, "error.kind", kind);
            } else {
                e.message = Some("turn completed".into());
            }
            put(
                &mut e.attributes,
                "prompt.id",
                safe_source_id(obj.get("prompt_id")),
            );
            out.events.push(e);
        }
        "SubagentStart" | "SubagentStop" => {
            let stop = hook == "SubagentStop";
            let derived = subagent
                .as_ref()
                .map(|a| format!("subagent:{a}:{}", if stop { "stop" } else { "start" }));
            let mut e = b.event(
                if stop {
                    "agent.subagent.stopped"
                } else {
                    "agent.subagent.started"
                },
                derived,
            );
            e.operation_id = subagent.as_ref().map(|a| format!("subagent:{a}"));
            e.message = Some(format!(
                "subagent {} ({})",
                if stop { "stopped" } else { "started" },
                subagent_type.as_deref().unwrap_or("unknown type")
            ));
            put(&mut e.attributes, "agent.subagent_id", subagent.clone());
            put(&mut e.attributes, "agent.type", subagent_type);
            out.events.push(e);
        }
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            tool_events(
                &mut b,
                obj,
                &hook,
                subagent.as_deref(),
                project_root.as_deref(),
                &mut out,
            );
        }
        other => {
            out.ignored = Some(format!("hook event {other} is not recorded"));
        }
    }
    Ok(out)
}

/// Without Git (or between session boundaries), the working directory is the
/// best available project hint; the daemon's session row keeps the Git root
/// learned at session start.
fn project_root_hint(cwd: &Path) -> Option<PathBuf> {
    Some(cwd.to_path_buf())
}

fn tool_events(
    b: &mut EventBuilder<'_>,
    obj: &serde_json::Map<String, Value>,
    hook: &str,
    subagent: Option<&str>,
    project_root: Option<&Path>,
    out: &mut Normalized,
) {
    let tool = safe_token(obj.get("tool_name"), 128).unwrap_or_else(|| "unknown".into());
    let use_id = safe_source_id(obj.get("tool_use_id")).map(str::to_string);
    let input = obj.get("tool_input").cloned().unwrap_or(Value::Null);
    let response = obj.get("tool_response");
    let duration = obj.get("duration_ms").and_then(Value::as_u64);
    let phase = match hook {
        "PreToolUse" => "pre",
        "PostToolUse" => "post",
        _ => "fail",
    };
    let tool_op = use_id.as_ref().map(|u| format!("tool:{u}"));
    let parent = subagent.map(|a| format!("subagent:{a}"));

    // agent.tool.*
    let (ty, status, severity) = match phase {
        "pre" => ("agent.tool.started", EventStatus::Started, LogLevel::Info),
        "post" => (
            "agent.tool.completed",
            EventStatus::Succeeded,
            LogLevel::Info,
        ),
        _ => {
            let interrupted = obj.get("is_interrupt").and_then(Value::as_bool) == Some(true);
            (
                "agent.tool.failed",
                if interrupted {
                    EventStatus::Cancelled
                } else {
                    EventStatus::Failed
                },
                if interrupted {
                    LogLevel::Warn
                } else {
                    LogLevel::Error
                },
            )
        }
    };
    let mut e = b.event(ty, use_id.as_ref().map(|u| format!("tool:{u}:{phase}")));
    e.operation_id.clone_from(&tool_op);
    e.parent_operation_id.clone_from(&parent);
    e.status = Some(status);
    e.severity = severity;
    if phase != "pre" {
        e.duration_ms = duration;
    }
    e.message = Some(match (phase, duration) {
        ("pre", _) => format!("{tool} started"),
        ("post", Some(d)) => format!("{tool} completed in {d} ms"),
        ("post", None) => format!("{tool} completed"),
        (_, _) if status == EventStatus::Cancelled => format!("{tool} interrupted"),
        (_, _) => format!("{tool} failed"),
    });
    put(&mut e.attributes, "tool.name", Some(tool.clone()));
    put(&mut e.attributes, "tool.use_id", use_id.clone());
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
    put(
        &mut e.attributes,
        "agent.subagent_id",
        subagent.map(str::to_string),
    );
    if phase == "fail" {
        if let Some(err) = obj.get("error").and_then(Value::as_str) {
            // The error text can quote command output: payload only.
            e.payload = Some(err.to_string());
        }
    }
    out.events.push(e);

    // command.*
    if is_shell_tool(&tool) {
        let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
        let summary = summarize_command(cmd);
        let interrupted = response
            .and_then(|r| r.get("interrupted"))
            .and_then(Value::as_bool)
            == Some(true)
            || obj.get("is_interrupt").and_then(Value::as_bool) == Some(true);
        let (cty, cstatus, csev) = match phase {
            "pre" => ("command.started", EventStatus::Started, LogLevel::Info),
            "post" if interrupted => ("command.failed", EventStatus::Cancelled, LogLevel::Warn),
            "post" => ("command.completed", EventStatus::Succeeded, LogLevel::Info),
            _ if interrupted => ("command.failed", EventStatus::Cancelled, LogLevel::Warn),
            _ => ("command.failed", EventStatus::Failed, LogLevel::Error),
        };
        let mut c = b.event(cty, use_id.as_ref().map(|u| format!("cmd:{u}:{phase}")));
        c.operation_id = use_id.as_ref().map(|u| format!("cmd:{u}"));
        c.parent_operation_id.clone_from(&tool_op);
        c.status = Some(cstatus);
        c.severity = csev;
        if phase != "pre" {
            c.duration_ms = duration;
        }
        let program = summary.program.clone().unwrap_or_else(|| "?".into());
        let exit = if phase == "fail" {
            obj.get("error")
                .and_then(Value::as_str)
                .and_then(exit_code_from_error)
        } else {
            None
        };
        c.message = Some(match (cstatus, exit) {
            (EventStatus::Started, _) => format!("command `{program}` started"),
            (EventStatus::Succeeded, _) => format!("command `{program}` completed"),
            (EventStatus::Cancelled, _) => format!("command `{program}` interrupted"),
            (_, Some(code)) => format!("command `{program}` failed (exit {code})"),
            (_, None) => format!("command `{program}` failed"),
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
                Some("error_text"),
            );
        }
        if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            c.attributes
                .insert("command.background".into(), Value::Bool(true));
        }
        if !cmd.is_empty() {
            // Dropped in `metadata` mode, redacted in `redacted` mode.
            c.payload = Some(cmd.to_string());
        }
        out.events.push(c);
    }

    // file.* — only for a successful call the tool itself reported.
    if phase == "post" {
        if let Some((raw, fty, uncertainty)) = file_effect(&tool, &input, response) {
            if let Some(path) = normalize_path(raw, b.cwd.as_deref()) {
                let mut f = b.event(fty, use_id.as_ref().map(|u| format!("file:{u}")));
                f.parent_operation_id.clone_from(&tool_op);
                f.path = Some(path.display().to_string());
                let verb = fty.trim_start_matches("file.");
                f.message = Some(format!(
                    "{verb} {} (reported by {tool})",
                    display_relative(&path, project_root)
                ));
                put(&mut f.attributes, "tool.name", Some(tool.clone()));
                put(&mut f.attributes, "tool.use_id", use_id.clone());
                put(&mut f.attributes, "attribution", Some("agent_reported"));
                f.attributes
                    .insert("verified_on_disk".into(), Value::Bool(false));
                put(&mut f.attributes, "file.change", uncertainty);
                out.events.push(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> HookContext {
        HookContext {
            received_at: chrono::Utc::now(),
            nonce: "n1".into(),
        }
    }

    fn run(v: Value) -> Normalized {
        normalize(&v, &ctx()).unwrap()
    }

    fn types(n: &Normalized) -> Vec<&str> {
        n.events.iter().map(|e| e.event_type.as_str()).collect()
    }

    const SID: &str = "0d2d9d6e-3f5f-4c2b-9f0e-1f2a3b4c5d6e";

    #[test]
    fn bash_lifecycle_pairs_by_tool_use_id() {
        let pre = run(
            json!({"hook_event_name":"PreToolUse","session_id":SID,"cwd":"/w/p",
            "tool_name":"Bash","tool_use_id":"toolu_1",
            "tool_input":{"command":"GITHUB_TOKEN=ghp_x cargo test","description":"Run tests"}}),
        );
        assert_eq!(types(&pre), ["agent.tool.started", "command.started"]);
        let fail = run(
            json!({"hook_event_name":"PostToolUseFailure","session_id":SID,"cwd":"/w/p",
            "tool_name":"Bash","tool_use_id":"toolu_1","duration_ms":1500,
            "tool_input":{"command":"cargo test"},"error":"Exit code 101\nerror: test failed"}),
        );
        assert_eq!(types(&fail), ["agent.tool.failed", "command.failed"]);
        let cmd = &fail.events[1];
        assert_eq!(cmd.attributes["command.exit_code"], 101);
        assert_eq!(cmd.attributes["command.program"], "cargo");
        assert_eq!(cmd.duration_ms, Some(1500));
        assert_eq!(cmd.operation_id.as_deref(), Some("cmd:toolu_1"));
        assert_eq!(cmd.parent_operation_id.as_deref(), Some("tool:toolu_1"));
        assert_eq!(pre.events[0].operation_id, fail.events[0].operation_id);
        assert_ne!(pre.events[0].event_id, fail.events[0].event_id);
        // The description is model text and is never copied.
        let all = serde_json::to_string(&pre.events).unwrap();
        assert!(!all.contains("Run tests"));
        for e in pre.events.iter().chain(&fail.events) {
            e.validate().unwrap();
            assert!(e.stable_id);
            assert_eq!(
                e.session_id.as_deref(),
                Some(&*format!("claude-code:{SID}"))
            );
        }
    }

    #[test]
    fn derived_ids_are_replay_stable() {
        let v = json!({"hook_event_name":"PostToolUse","session_id":SID,"cwd":"/w",
            "tool_name":"Read","tool_use_id":"toolu_9","tool_input":{"file_path":"/w/a.rs"}});
        let a = normalize(&v, &ctx()).unwrap();
        let b = normalize(
            &v,
            &HookContext {
                received_at: chrono::Utc::now(),
                nonce: "other".into(),
            },
        )
        .unwrap();
        let ids = |n: &Normalized| {
            n.events
                .iter()
                .map(|e| e.event_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&a),
            ids(&b),
            "duplicate delivery of one tool call deduplicates"
        );
        assert_eq!(types(&a), ["agent.tool.completed", "file.read"]);
        assert_eq!(a.events[1].path.as_deref(), Some("/w/a.rs"));
    }

    #[test]
    fn write_distinguishes_create_from_update_and_unknown() {
        let w = |resp: Value| {
            run(
                json!({"hook_event_name":"PostToolUse","session_id":SID,"cwd":"/w",
            "tool_name":"Write","tool_use_id":"t","tool_input":{"file_path":"x.txt","content":"SECRET"},
            "tool_response":resp}),
            )
        };
        assert_eq!(
            w(json!({"type":"create"})).events[1].event_type,
            "file.created"
        );
        assert_eq!(
            w(json!({"type":"update"})).events[1].event_type,
            "file.modified"
        );
        let unknown = w(Value::Null);
        assert_eq!(
            unknown.events[1].attributes["file.change"],
            "create_or_update"
        );
        assert_eq!(
            unknown.events[1].path.as_deref(),
            Some("/w/x.txt"),
            "relative to cwd"
        );
        assert!(!serde_json::to_string(&unknown.events)
            .unwrap()
            .contains("SECRET"));
    }

    #[test]
    fn failed_file_tools_claim_no_file_change() {
        let n = run(
            json!({"hook_event_name":"PostToolUseFailure","session_id":SID,"cwd":"/w",
            "tool_name":"Edit","tool_use_id":"t","tool_input":{"file_path":"/w/a"},"error":"not found"}),
        );
        assert_eq!(types(&n), ["agent.tool.failed"]);
    }

    #[test]
    fn session_and_subagent_events() {
        let s = run(
            json!({"hook_event_name":"SessionStart","session_id":SID,"cwd":"/nonexistent/x",
            "source":"resume","model":"claude-opus-5-5"}),
        );
        assert_eq!(types(&s), ["agent.session.started"]);
        assert_eq!(s.events[0].attributes["session.source"], "resume");
        let sub = run(
            json!({"hook_event_name":"SubagentStart","session_id":SID,"cwd":"/w",
            "agent_id":"a1b2","agent_type":"Explore"}),
        );
        assert_eq!(sub.events[0].operation_id.as_deref(), Some("subagent:a1b2"));
        let inner = run(
            json!({"hook_event_name":"PreToolUse","session_id":SID,"cwd":"/w",
            "agent_id":"a1b2","tool_name":"Grep","tool_use_id":"t2","tool_input":{}}),
        );
        assert_eq!(
            inner.events[0].parent_operation_id.as_deref(),
            Some("subagent:a1b2")
        );
        let end = run(
            json!({"hook_event_name":"SessionEnd","session_id":SID,"reason":"prompt_input_exit"}),
        );
        assert_eq!(
            end.events[0].attributes["session.end_reason"],
            "prompt_input_exit"
        );
        assert_eq!(end.events[0].path, None, "no cwd, no path");
    }

    #[test]
    fn prompts_and_turns_record_metadata_only() {
        let p = run(
            json!({"hook_event_name":"UserPromptSubmit","session_id":SID,"prompt":"my password is hunter2"}),
        );
        assert_eq!(p.events[0].attributes["prompt.chars"], 22);
        assert!(!serde_json::to_string(&p.events)
            .unwrap()
            .contains("hunter2"));
        let t = run(
            json!({"hook_event_name":"StopFailure","session_id":SID,"error":"rate_limit",
            "last_assistant_message":"API Error: secret"}),
        );
        assert_eq!(t.events[0].event_type, "agent.turn.failed");
        assert!(!serde_json::to_string(&t.events).unwrap().contains("secret"));
    }

    #[test]
    fn tolerant_of_missing_and_unknown_fields() {
        let n =
            run(json!({"hook_event_name":"PreToolUse","tool_name":"Bash","future_field":{"x":1}}));
        assert_eq!(types(&n), ["agent.tool.started", "command.started"]);
        assert_eq!(n.events[0].session_id, None);
        assert_ne!(n.events[0].event_id, n.events[1].event_id);
        let ignored = run(json!({"hook_event_name":"Notification","session_id":SID}));
        assert!(ignored.events.is_empty());
        assert!(ignored.ignored.is_some());
        let future = run(json!({"hook_event_name":"SomethingNew2027"}));
        assert!(future.events.is_empty());
    }

    #[test]
    fn malformed_inputs_are_errors_not_panics() {
        for v in [
            json!(null),
            json!([1]),
            json!("x"),
            json!({}),
            json!({"hook_event_name": 5}),
            json!({"hook_event_name": "Bad Name; rm -rf"}),
        ] {
            assert!(normalize(&v, &ctx()).is_err(), "{v}");
        }
        // Hostile ids never reach relationships.
        let n = run(json!({"hook_event_name":"PostToolUse","tool_name":"Bash",
            "tool_use_id":"x y\nz","agent_id":"../../etc"}));
        assert_eq!(n.events[0].operation_id, None);
        assert_eq!(n.events[0].parent_operation_id, None);
    }

    #[test]
    fn exit_codes_parse_only_the_documented_prefix() {
        assert_eq!(exit_code_from_error("Exit code 1"), Some(1));
        assert_eq!(exit_code_from_error("Exit code 127\nnot found"), Some(127));
        assert_eq!(exit_code_from_error("Error: Exit code 1"), None);
        assert_eq!(exit_code_from_error("Exit code x"), None);
    }
}
