//! Replays recorded hook inputs (see `fixtures/README.md`) through the
//! adapters. The Claude Code corpus is real 2.1.331 output, so these tests
//! catch drift between what TailFlow expects and what the agent sends.

use serde_json::Value;
use std::path::Path;
use tailflow_core::agent::{normalize, AgentKind, HookContext};
use tailflow_core::event::ObservabilityEvent;

fn corpus(dir: &str) -> Vec<(String, Value)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir);
    let mut files: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (
                name,
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap(),
            )
        })
        .collect()
}

fn replay(kind: AgentKind, dir: &str) -> Vec<(String, Vec<ObservabilityEvent>)> {
    corpus(dir)
        .into_iter()
        .map(|(name, v)| {
            let n =
                normalize(kind, &v, &HookContext::now()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                n.ignored.is_none(),
                "{name}: every installed event is recorded"
            );
            for e in &n.events {
                e.validate().unwrap_or_else(|err| panic!("{name}: {err}"));
                assert!(e.stable_id);
            }
            (name, n.events)
        })
        .collect()
}

fn types(events: &[ObservabilityEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event_type.as_str()).collect()
}

#[test]
fn claude_code_2_1_331_corpus_normalizes() {
    let out = replay(AgentKind::ClaudeCode, "claude-code-2.1.331");
    assert_eq!(out.len(), 23);
    let all: Vec<&ObservabilityEvent> = out.iter().flat_map(|(_, e)| e).collect();
    let json = serde_json::to_string(&all).unwrap();
    assert!(
        !json.contains("fixture prompt"),
        "prompt text is never copied"
    );
    assert!(
        !json.contains("fixture assistant"),
        "assistant text is never copied"
    );
    assert!(
        !json.contains("transcript"),
        "transcript paths are not recorded"
    );

    let by = |name: &str| &out.iter().find(|(n, _)| n == name).unwrap().1;
    // Real non-zero Bash exit: PostToolUseFailure with "Exit code 3".
    let fail = by("14-PostToolUseFailure.json");
    assert_eq!(types(fail), ["agent.tool.failed", "command.failed"]);
    assert_eq!(fail[1].attributes["command.exit_code"], 3);
    assert!(fail[0].duration_ms.is_some());
    // Real Write response: type "create".
    assert!(out.iter().any(|(_, e)| types(e).contains(&"file.created")));
    assert!(out.iter().any(|(_, e)| types(e).contains(&"file.modified")));
    assert!(out.iter().any(|(_, e)| types(e).contains(&"file.read")));
    // Subagent: start/stop share an operation; its tool calls point at it.
    let start = &by("02-SubagentStart.json")[0];
    let stop = &by("07-SubagentStop.json")[0];
    assert!(start.operation_id.is_some());
    assert_eq!(start.operation_id, stop.operation_id);
    let inner = all
        .iter()
        .filter(|e| e.parent_operation_id == start.operation_id)
        .count();
    assert!(inner >= 2, "the subagent's tool calls are linked to it");
    // Pre/Post of one call share the tool operation.
    let pre = &by("13-PreToolUse.json")[0];
    assert_eq!(pre.operation_id, fail[0].operation_id);
    // Two sessions, each with exactly one start and one end.
    let mut sessions: Vec<&str> = all.iter().filter_map(|e| e.session_id.as_deref()).collect();
    sessions.sort_unstable();
    sessions.dedup();
    assert_eq!(sessions.len(), 2);
}

#[test]
fn codex_synthetic_corpus_normalizes() {
    let out = replay(AgentKind::Codex, "codex-0.172.0-synthetic");
    let all: Vec<&ObservabilityEvent> = out.iter().flat_map(|(_, e)| e).collect();
    let t: Vec<&str> = all.iter().map(|e| e.event_type.as_str()).collect();
    for want in [
        "agent.session.started",
        "agent.prompt.submitted",
        "command.started",
        "file.modified",
        "file.created",
        "agent.subagent.started",
        "agent.model.completed",
        "agent.turn.completed",
        "agent.session.ended",
    ] {
        assert!(t.contains(&want), "{want} missing: {t:?}");
    }
    assert!(
        !t.contains(&"command.completed"),
        "no exit code in the response: no outcome claimed"
    );
    let model = all
        .iter()
        .find(|e| e.event_type == "agent.model.completed")
        .unwrap();
    assert_eq!(model.attributes["tokens.total"], 1350);
    let mcp = all
        .iter()
        .find(|e| e.attributes.get("tool.mcp_server").is_some())
        .unwrap();
    assert_eq!(mcp.parent_operation_id.as_deref(), Some("subagent:agent-1"));
}
