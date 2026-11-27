//! Where a hook call's time goes, in-process (no spawn): policy compile,
//! normalize, sanitize, spool write. `cargo run --release --example hook_cost`.
use std::time::Instant;
use tailflow_core::agent::{normalize, AgentKind, HookContext};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig, Redactor};

fn main() {
    let t = Instant::now();
    let r = Redactor::builtin();
    let builtin = t.elapsed();
    let t = Instant::now();
    let p = CapturePolicy::from_config(&PrivacyConfig::default()).unwrap();
    let policy = t.elapsed();
    let v: serde_json::Value = serde_json::from_str(
        r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/w","tool_name":"Bash","tool_use_id":"t","tool_input":{"command":"cargo test"}}"#,
    )
    .unwrap();
    let t = Instant::now();
    let mut n = normalize(AgentKind::ClaudeCode, &v, &HookContext::now()).unwrap();
    let norm = t.elapsed();
    let t = Instant::now();
    for e in &mut n.events {
        p.admit(e).unwrap();
        p.sanitize(e);
    }
    let san = t.elapsed();
    let t = Instant::now();
    let _ = r.redact("Bash started");
    let first = t.elapsed();
    println!("Redactor::builtin {builtin:?}  CapturePolicy {policy:?}  normalize {norm:?}  sanitize {san:?}  first redact {first:?}");
}
