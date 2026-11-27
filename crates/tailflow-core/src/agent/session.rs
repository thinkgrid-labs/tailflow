//! Session identity.
//!
//! A TailFlow session key is `<agent>:<source session id>`:
//!
//! - **Source ids only.** Sessions are never inferred from timestamps,
//!   process ids or a shared project directory: two agents working in the
//!   same repository at the same moment stay separate because their source
//!   ids differ.
//! - **Namespaced.** A Codex thread and a Claude Code session with the same
//!   UUID are different keys.
//! - **Restart-proof.** The key is derived from the source id alone, so it is
//!   identical across daemon restarts, spool replays and resumed sessions
//!   (when the agent keeps the id on resume).
//!
//! A source id with unexpected characters or length is hashed instead of
//! dropped, so the session is still tracked without letting arbitrary text
//! into an indexed key.

use super::{fnv1a64, AgentKind};

/// Longest source id kept verbatim.
pub const MAX_SOURCE_SESSION_ID: usize = 160;

/// The session key for `source_id` from `agent`, or `None` when the source
/// supplied no id at all.
pub fn session_key(agent: AgentKind, source_id: &str) -> Option<String> {
    let id = source_id.trim();
    if id.is_empty() {
        return None;
    }
    let plain = id.len() <= MAX_SOURCE_SESSION_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    Some(if plain {
        format!("{}:{id}", agent.as_str())
    } else {
        let b = id.as_bytes();
        format!(
            "{}:h-{:016x}{:016x}",
            agent.as_str(),
            fnv1a64(b, 0),
            fnv1a64(b, 0x9e37_79b9_7f4a_7c15)
        )
    })
}

/// Split a session key into `(agent, source id)`.
pub fn parse_session_key(key: &str) -> Option<(AgentKind, &str)> {
    let (agent, rest) = key.split_once(':')?;
    let kind = AgentKind::parse(agent)?;
    (!rest.is_empty()).then_some((kind, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_and_stable() {
        let id = "8c0a1f9e-4b1d-4e0a-9f55-2f4f7a9d1c10";
        let c = session_key(AgentKind::ClaudeCode, id).unwrap();
        let x = session_key(AgentKind::Codex, id).unwrap();
        assert_eq!(c, format!("claude-code:{id}"));
        assert_ne!(c, x);
        assert_eq!(c, session_key(AgentKind::ClaudeCode, id).unwrap());
        assert_eq!(parse_session_key(&c), Some((AgentKind::ClaudeCode, id)));
    }

    #[test]
    fn odd_ids_are_hashed_not_dropped() {
        let k = session_key(AgentKind::ClaudeCode, "has space/and\nnewline").unwrap();
        assert!(k.starts_with("claude-code:h-"));
        assert!(k.len() < 64);
        let long = "a".repeat(500);
        assert!(session_key(AgentKind::Codex, &long)
            .unwrap()
            .starts_with("codex:h-"));
        assert_eq!(session_key(AgentKind::Codex, "  "), None);
    }
}
