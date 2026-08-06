//! The four operations both binaries expose, in one place.
//!
//! `tailflow-mcp` reaches these from JSON-RPC tool calls and `tailflow-logs`
//! from CLI flags; keeping the request construction here is what guarantees an
//! agent gets identical results whichever door it comes through.

use crate::client::{ClientError, DaemonClient, QueryString};
use serde_json::Value;
use std::time::Duration;

/// Ordinary requests are served from an in-memory ring buffer over loopback;
/// anything slower than this is a hung daemon, not a slow query.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Slack added on top of a long-poll's own deadline, so the client always
/// outlives the server-side wait it asked for.
const WAIT_SLACK: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Clone)]
pub struct ErrorsArgs {
    pub grep: Option<String>,
    pub source: Option<String>,
    pub level: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
    pub context_lines: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct SearchArgs {
    pub grep: Option<String>,
    pub source: Option<String>,
    pub level: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct WaitArgs {
    pub grep: Option<String>,
    pub source: Option<String>,
    pub level: Option<String>,
    pub timeout_ms: Option<u64>,
    pub cursor: Option<u64>,
    pub limit: Option<usize>,
}

pub async fn errors(client: &DaemonClient, args: &ErrorsArgs) -> Result<Value, ClientError> {
    let mut qs = QueryString::new();
    qs.push_opt("grep", args.grep.as_ref())
        .push_opt("source", args.source.as_ref())
        .push_opt("level", args.level.as_ref())
        .push_opt("since", args.since.as_ref())
        .push_num("limit", args.limit)
        .push_num("context_lines", args.context_lines);
    client.get(&qs.build("/api/errors"), REQUEST_TIMEOUT).await
}

pub async fn search(client: &DaemonClient, args: &SearchArgs) -> Result<Value, ClientError> {
    let mut qs = QueryString::new();
    qs.push_opt("grep", args.grep.as_ref())
        .push_opt("source", args.source.as_ref())
        .push_opt("level", args.level.as_ref())
        .push_opt("since", args.since.as_ref())
        .push_num("limit", args.limit)
        .push_num("cursor", args.cursor);
    client.get(&qs.build("/api/query"), REQUEST_TIMEOUT).await
}

pub async fn sources(client: &DaemonClient) -> Result<Value, ClientError> {
    client.get("/api/sources", REQUEST_TIMEOUT).await
}

pub async fn wait(client: &DaemonClient, args: &WaitArgs) -> Result<Value, ClientError> {
    let timeout_ms = args.timeout_ms.unwrap_or(30_000);
    let mut qs = QueryString::new();
    qs.push_opt("grep", args.grep.as_ref())
        .push_opt("source", args.source.as_ref())
        .push_opt("level", args.level.as_ref())
        .push_num("cursor", args.cursor)
        .push_num("limit", args.limit)
        .push("timeout_ms", timeout_ms.to_string());
    client
        .get(
            &qs.build("/api/wait"),
            Duration::from_millis(timeout_ms) + WAIT_SLACK,
        )
        .await
}

pub async fn health(client: &DaemonClient) -> Result<Value, ClientError> {
    client.get("/health", REQUEST_TIMEOUT).await
}
