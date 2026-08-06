//! Model Context Protocol server over stdio.
//!
//! Implements the JSON-RPC 2.0 subset MCP needs for a tools-only server:
//! `initialize`, `tools/list`, `tools/call`, `ping`, and the `initialized`
//! notification. That is a few hundred lines against a stable wire format —
//! cheaper than an SDK dependency for two binaries that ship over npm.

use crate::client::{ClientError, DaemonClient};
use crate::ops::{self, ErrorsArgs, SearchArgs, WaitArgs};
use crate::render;
use serde_json::{json, Value};

/// Protocol revisions this server speaks. The client's requested version is
/// echoed back when we know it, per the negotiation rule in the spec.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
pub const LATEST_PROTOCOL: &str = "2025-06-18";

pub const SERVER_NAME: &str = "tailflow";

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub struct Server {
    client: DaemonClient,
}

impl Server {
    pub fn new(client: DaemonClient) -> Self {
        Self { client }
    }

    /// Handle one incoming message.
    ///
    /// Returns `None` for notifications — a JSON-RPC notification has no `id`
    /// and must not be answered. Replying to one desynchronises strict clients.
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        let method = msg.get("method").and_then(Value::as_str)?.to_string();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // No id → notification. Nothing to do for any we currently accept.
        let id = match id {
            Some(id) if !id.is_null() => id,
            _ => return None,
        };

        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.call_tool(&params).await,
            "ping" => Ok(json!({})),
            other => Err(RpcError {
                code: METHOD_NOT_FOUND,
                message: format!("unknown method: {other}"),
            }),
        };

        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": e.code, "message": e.message }
            }),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = match requested {
            Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => v,
            _ => LATEST_PROTOCOL,
        };

        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": format!(
                "Reads the live local development stack aggregated by a TailFlow daemon at {}. \
                 Use it to see what a running service actually printed — build failures, \
                 request errors, panics, stack traces — instead of guessing from source code. \
                 After changing code and triggering a rebuild or request, call wait_for_logs to \
                 be woken the moment something matches, then get_recent_errors to read what \
                 broke. Timestamps are local to the developer's machine.",
                self.client.url()
            ),
        })
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "tools/call requires a `name`".into(),
            })?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        // A tool that fails is reported *inside* a successful result with
        // `isError: true`, not as a JSON-RPC error. That is what puts the
        // failure text in front of the model so it can react — a protocol
        // error is handled by the client and the model never sees it.
        let outcome = match name {
            "list_log_sources" => self.tool_sources().await,
            "get_recent_errors" => self.tool_errors(&args).await,
            "search_logs" => self.tool_search(&args).await,
            "wait_for_logs" => self.tool_wait(&args).await,
            other => {
                return Err(RpcError {
                    code: INVALID_PARAMS,
                    message: format!("unknown tool: {other}"),
                })
            }
        };

        Ok(match outcome {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
            Err(e) => json!({
                "content": [{ "type": "text", "text": e.to_string() }],
                "isError": true,
            }),
        })
    }

    // ── Tools ─────────────────────────────────────────────────────────────────

    async fn tool_sources(&self) -> Result<String, ClientError> {
        let v = ops::sources(&self.client).await?;
        Ok(render::sources(&v))
    }

    async fn tool_errors(&self, args: &Value) -> Result<String, ClientError> {
        let a = ErrorsArgs {
            grep: str_arg(args, "grep"),
            source: str_arg(args, "source"),
            level: str_arg(args, "level"),
            since: str_arg(args, "since"),
            limit: num_arg(args, "limit").map(|n| n as usize),
            context_lines: num_arg(args, "context_lines").map(|n| n as usize),
        };
        let v = ops::errors(&self.client, &a).await?;
        Ok(finish(args, v, render::errors))
    }

    async fn tool_search(&self, args: &Value) -> Result<String, ClientError> {
        let a = SearchArgs {
            grep: str_arg(args, "grep"),
            source: str_arg(args, "source"),
            level: str_arg(args, "level"),
            since: str_arg(args, "since"),
            limit: num_arg(args, "limit").map(|n| n as usize),
            cursor: num_arg(args, "cursor"),
        };
        let v = ops::search(&self.client, &a).await?;
        Ok(finish(args, v, render::records))
    }

    async fn tool_wait(&self, args: &Value) -> Result<String, ClientError> {
        let a = WaitArgs {
            grep: str_arg(args, "grep"),
            source: str_arg(args, "source"),
            level: str_arg(args, "level"),
            timeout_ms: num_arg(args, "timeout_ms"),
            cursor: num_arg(args, "cursor"),
            limit: num_arg(args, "limit").map(|n| n as usize),
        };
        let v = ops::wait(&self.client, &a).await?;
        Ok(finish(args, v, render::wait))
    }
}

/// Render as compact text unless the caller explicitly asked for JSON.
fn finish(args: &Value, v: Value, renderer: fn(&Value) -> String) -> String {
    if str_arg(args, "format").as_deref() == Some("json") {
        serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("serialization failed: {e}"))
    } else {
        renderer(&v)
    }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn num_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| {
        v.as_u64()
            // Some clients stringify numbers; accept that rather than silently
            // dropping the argument.
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Build a JSON-RPC error for a message that could not even be parsed.
pub fn parse_error(detail: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": PARSE_ERROR, "message": format!("invalid JSON: {detail}") }
    })
}

pub fn invalid_request(detail: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": INVALID_REQUEST, "message": detail }
    })
}

// ── Tool schemas ──────────────────────────────────────────────────────────────

const SINCE_DESC: &str = "How far back to look: a relative duration ('30s', '5m', '2h', '1d') or \
                          an RFC 3339 timestamp. Prefer the relative form — '2m' after you \
                          triggered a rebuild scopes the answer to your own change.";
const LEVEL_DESC: &str = "Minimum severity: trace, debug, info, warn, or error. Lines with no \
                          detectable level are excluded by any setting.";
const FORMAT_DESC: &str = "'text' (default, compact) or 'json' for the raw daemon response.";

pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_log_sources",
            "description":
                "List every service the TailFlow daemon is capturing, with record, error and \
                 warning counts and each one's most recent line. Call this first: it tells you \
                 what is actually running, and distinguishes 'the service is quiet' from 'the \
                 service never started' — which look identical in an empty error list.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_recent_errors",
            "description":
                "The distinct failures in the running stack, deduplicated. Identical errors are \
                 collapsed into one group with an occurrence count, so 400 repeats of one crash \
                 loop read as a single entry, and each group carries the stack trace that \
                 followed it. This is the right first call for 'did my change break anything?' — \
                 use search_logs only when you need individual lines.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": { "type": "string", "description": SINCE_DESC },
                    "source": { "type": "string", "description":
                        "Only this service. Substring match, so 'api' matches 'reko-api'." },
                    "level": { "type": "string", "description":
                        "Minimum severity. Defaults to 'error'; pass 'warn' to widen." },
                    "grep": { "type": "string", "description":
                        "Additional regex the line must match." },
                    "limit": { "type": "integer", "description":
                        "Maximum distinct groups to return (default 20)." },
                    "context_lines": { "type": "integer", "description":
                        "Trailing stack-trace lines per group (default 8, 0 to omit)." },
                    "format": { "type": "string", "enum": ["text", "json"],
                                "description": FORMAT_DESC }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "search_logs",
            "description":
                "Individual log lines matching a filter, oldest first. Use when you need the \
                 exact sequence of events — a request's lifecycle, startup order, what a service \
                 printed just before it died — rather than a deduplicated failure summary. \
                 Every response ends with a cursor; pass it back as 'cursor' to read only what \
                 has arrived since.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "grep": { "type": "string", "description":
                        "Regex matched against the line body (not the service name)." },
                    "source": { "type": "string", "description":
                        "Only this service. Substring match." },
                    "level": { "type": "string", "description": LEVEL_DESC },
                    "since": { "type": "string", "description": SINCE_DESC },
                    "limit": { "type": "integer", "description":
                        "Maximum lines (default 100, max 1000). Newest are kept when truncating." },
                    "cursor": { "type": "integer", "description":
                        "Return only lines newer than this sequence number, from a previous \
                         response. Use it to poll incrementally instead of re-reading." },
                    "format": { "type": "string", "enum": ["text", "json"],
                                "description": FORMAT_DESC }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "wait_for_logs",
            "description":
                "Block until a matching line appears, then return it — instead of sleeping and \
                 polling. Call this right after you trigger something asynchronous (a hot \
                 reload, a rebuild, a request, a container restart) to be woken the instant it \
                 succeeds or fails. Returns as soon as the first match lands, plus the burst \
                 that follows it, or reports that nothing matched before the timeout.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "grep": { "type": "string", "description":
                        "Regex the line must match, e.g. 'compiled successfully|Failed to compile'." },
                    "source": { "type": "string", "description":
                        "Only watch this service. Substring match." },
                    "level": { "type": "string", "description":
                        "Only wake on lines at or above this severity, e.g. 'error'." },
                    "timeout_ms": { "type": "integer", "description":
                        "How long to wait before giving up (default 30000, max 120000)." },
                    "cursor": { "type": "integer", "description":
                        "Ignore anything at or before this sequence number. Take it from an \
                         earlier response so you match only events caused by your action." },
                    "limit": { "type": "integer", "description":
                        "Maximum lines to return (default 100)." },
                    "format": { "type": "string", "enum": ["text", "json"],
                                "description": FORMAT_DESC }
                },
                "additionalProperties": false
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        // Port 1 is never bound; these tests exercise protocol handling only.
        Server::new(DaemonClient::new("http://127.0.0.1:1"))
    }

    #[tokio::test]
    async fn initialize_echoes_a_supported_protocol_version() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2024-11-05" }
            }))
            .await
            .unwrap();
        assert_eq!(res["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(res["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(res["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn initialize_falls_back_to_latest_for_unknown_version() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "1999-01-01" }
            }))
            .await
            .unwrap();
        assert_eq!(res["result"]["protocolVersion"], LATEST_PROTOCOL);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let res = server()
            .handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        assert!(res.is_none(), "notifications must not be answered");
    }

    #[tokio::test]
    async fn tools_list_advertises_all_four_tools() {
        let res = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let tools = res["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "list_log_sources",
                "get_recent_errors",
                "search_logs",
                "wait_for_logs"
            ]
        );
        for t in tools {
            assert!(t["description"].as_str().unwrap().len() > 80);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[tokio::test]
    async fn unknown_method_is_a_protocol_error() {
        let res = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }))
            .await
            .unwrap();
        assert_eq!(res["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_tool_is_a_protocol_error() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "drop_database" }
            }))
            .await
            .unwrap();
        assert_eq!(res["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let res = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" }))
            .await
            .unwrap();
        assert_eq!(res["result"], json!({}));
    }

    #[tokio::test]
    async fn unreachable_daemon_is_a_tool_error_not_a_protocol_error() {
        // The model must see this text and be able to act on it.
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": { "name": "list_log_sources" }
            }))
            .await
            .unwrap();
        assert!(res.get("error").is_none(), "must not be a protocol error");
        assert_eq!(res["result"]["isError"], true);
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("No TailFlow daemon"), "got: {text}");
        assert!(text.contains("tailflow-daemon"), "must say how to fix it");
    }

    #[tokio::test]
    async fn id_is_preserved_across_types() {
        for id in [json!(7), json!("req-7")] {
            let res = server()
                .handle(json!({ "jsonrpc": "2.0", "id": id, "method": "ping" }))
                .await
                .unwrap();
            assert_eq!(res["id"], id);
            assert_eq!(res["jsonrpc"], "2.0");
        }
    }

    #[test]
    fn num_arg_accepts_stringified_numbers() {
        let args = json!({ "limit": "25", "timeout_ms": 500 });
        assert_eq!(num_arg(&args, "limit"), Some(25));
        assert_eq!(num_arg(&args, "timeout_ms"), Some(500));
        assert_eq!(num_arg(&args, "missing"), None);
    }

    #[test]
    fn str_arg_treats_empty_string_as_absent() {
        let args = json!({ "grep": "", "source": "api" });
        assert_eq!(str_arg(&args, "grep"), None);
        assert_eq!(str_arg(&args, "source"), Some("api".into()));
    }

    #[test]
    fn finish_honours_json_format() {
        let v = json!({ "groups": [], "distinct": 0 });
        let out = finish(&json!({ "format": "json" }), v.clone(), render::errors);
        assert!(out.starts_with('{'));
        let out = finish(&json!({}), v, render::errors);
        assert!(out.contains("No matching errors"));
    }
}
