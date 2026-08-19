//! Model Context Protocol server over stdio.
//!
//! A dual-era tools-only server: modern clients use per-request metadata and
//! `server/discover`; legacy clients use `initialize`. Keeping the wire layer
//! here avoids pulling the ingestion dependency tree into either agent binary.

use crate::client::{ClientError, DaemonClient};
use crate::ops::{self, ErrorsArgs, SearchArgs, WaitArgs};
use crate::render;
use serde_json::{json, Value};
use std::fmt;

/// Protocol revisions this server speaks. The client's requested version is
/// echoed back when we know it, per the negotiation rule in the spec.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const LATEST_PROTOCOL: &str = "2025-11-25";
pub const MODERN_PROTOCOL: &str = "2026-07-28";

pub const SERVER_NAME: &str = "tailflow";

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

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
        let id = msg.get("id").cloned();
        let method = match msg.get("method").and_then(Value::as_str) {
            Some(method) => method.to_string(),
            None => return Some(invalid_request("request requires a string `method`")),
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // No id → notification. Nothing to do for any we currently accept.
        let id = match id {
            Some(id) if !id.is_null() => id,
            _ => return None,
        };

        let requested_protocol = request_protocol(&params);
        if let Some(version) = requested_protocol {
            if version != MODERN_PROTOCOL {
                return Some(rpc_error_response(
                    id,
                    RpcError {
                        code: UNSUPPORTED_PROTOCOL_VERSION,
                        message: "Unsupported protocol version".into(),
                        data: Some(json!({
                            "supported": [MODERN_PROTOCOL, LATEST_PROTOCOL],
                            "requested": version,
                        })),
                    },
                ));
            }
            if !has_client_capabilities(&params) {
                return Some(rpc_error_response(
                    id,
                    RpcError {
                        code: MISSING_REQUIRED_CLIENT_CAPABILITY,
                        message: "Missing required client capability metadata".into(),
                        data: Some(json!({
                            "required": "io.modelcontextprotocol/clientCapabilities"
                        })),
                    },
                ));
            }
        }
        let modern = requested_protocol == Some(MODERN_PROTOCOL);

        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "server/discover" => Ok(self.discover()),
            "tools/list" => Ok(if modern {
                json!({ "tools": tool_definitions(), "ttlMs": 300_000, "cacheScope": "public" })
            } else {
                json!({ "tools": tool_definitions() })
            }),
            "tools/call" => self.call_tool(&params).await,
            "ping" => Ok(json!({})),
            other => Err(RpcError {
                code: METHOD_NOT_FOUND,
                message: format!("unknown method: {other}"),
                data: None,
            }),
        };

        Some(match result {
            Ok(mut result) => {
                if modern || method == "server/discover" {
                    stamp_modern_result(&mut result);
                }
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            Err(e) => rpc_error_response(id, e),
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
            "instructions": self.instructions(),
        })
    }

    fn discover(&self) -> Value {
        json!({
            "resultType": "complete",
            "supportedVersions": [MODERN_PROTOCOL],
            "capabilities": { "tools": { "listChanged": false } },
            "instructions": self.instructions(),
            "ttlMs": 300_000,
            "cacheScope": "public",
        })
    }

    fn instructions(&self) -> String {
        format!(
            "Reads the live local development stack aggregated by a TailFlow daemon at {}. \
             Use it to verify runtime behavior after code changes. Call list_log_sources first, \
             use wait_for_logs with a snapshot cursor for asynchronous work, and use \
             get_recent_errors for deduplicated failures.",
            self.client.url()
        )
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "tools/call requires a `name`".into(),
                data: None,
            })?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        // The advertised schema is also the dispatch table, so the tool list a
        // client sees and the tools this server actually runs cannot drift.
        let schema = tool_schema(name).ok_or_else(|| RpcError {
            code: INVALID_PARAMS,
            message: format!("unknown tool: {name}"),
            data: None,
        })?;

        // A tool that fails is reported *inside* a successful result with
        // `isError: true`, not as a JSON-RPC error. That is what puts the
        // failure text in front of the model so it can react — a protocol
        // error is handled by the client and the model never sees it.
        let outcome = match validate_args(name, &schema, &args) {
            Err(message) => Err(ToolFailure::Args(message)),
            Ok(()) => match name {
                "list_log_sources" => self.tool_sources(&args).await,
                "get_recent_errors" => self.tool_errors(&args).await,
                "search_logs" => self.tool_search(&args).await,
                "wait_for_logs" => self.tool_wait(&args).await,
                _ => unreachable!("tool_schema resolves only the names dispatched here"),
            },
        };

        Ok(match outcome {
            Ok(text) => json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }),
            Err(e) => json!({
                "content": [{ "type": "text", "text": e.to_string() }],
                "isError": true,
            }),
        })
    }

    // ── Tools ─────────────────────────────────────────────────────────────────

    async fn tool_sources(&self, args: &Value) -> Result<String, ToolFailure> {
        let v = ops::sources(&self.client).await?;
        Ok(finish(args, v, render::sources))
    }

    async fn tool_errors(&self, args: &Value) -> Result<String, ToolFailure> {
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

    async fn tool_search(&self, args: &Value) -> Result<String, ToolFailure> {
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

    async fn tool_wait(&self, args: &Value) -> Result<String, ToolFailure> {
        let a = WaitArgs {
            grep: str_arg(args, "grep"),
            source: str_arg(args, "source"),
            level: str_arg(args, "level"),
            since: str_arg(args, "since"),
            timeout_ms: num_arg(args, "timeout_ms"),
            cursor: num_arg(args, "cursor"),
            limit: num_arg(args, "limit").map(|n| n as usize),
            require_new: bool_arg(args, "require_new").unwrap_or(false),
        };
        let v = ops::wait(&self.client, &a).await?;
        Ok(finish(args, v, render::wait))
    }
}

/// Why a tool call failed.
///
/// Both variants reach the model as `isError: true` content rather than a
/// JSON-RPC error. A protocol error is consumed by the MCP client, so the model
/// would only see that its call produced nothing — indistinguishable from a
/// stack with nothing wrong in it.
pub enum ToolFailure {
    /// The daemon was unreachable, or rejected the request.
    Daemon(ClientError),
    /// The arguments were malformed. Rejected rather than partially applied:
    /// a filter that quietly vanishes turns a broken stack into an empty
    /// result, and an empty result reads as "all clear".
    Args(String),
}

impl From<ClientError> for ToolFailure {
    fn from(e: ClientError) -> Self {
        ToolFailure::Daemon(e)
    }
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolFailure::Daemon(e) => write!(f, "{e}"),
            ToolFailure::Args(message) => write!(f, "{message}"),
        }
    }
}

// ── Argument validation ───────────────────────────────────────────────────────

/// The `inputSchema` of one advertised tool, or `None` if no such tool exists.
fn tool_schema(name: &str) -> Option<Value> {
    tool_definitions()
        .as_array()?
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))?
        .get("inputSchema")
        .cloned()
}

/// Check `args` against the schema the client was given.
///
/// Every tool advertises `additionalProperties: false`, but nothing in MCP
/// obliges a client to enforce it, and the model on the other end is generating
/// these names from a description. Without this check a misspelled `pattern`,
/// a `since` passed to the one tool that has no `since`, or a stringified array
/// where a string belongs is simply not read: the request runs *unfiltered*,
/// succeeds, and returns an answer to a question nobody asked.
fn validate_args(tool: &str, schema: &Value, args: &Value) -> Result<(), String> {
    let supplied = match args {
        Value::Object(map) => map,
        Value::Null => return Ok(()),
        other => {
            return Err(format!(
                "`arguments` for {tool} must be a JSON object, got {}.",
                json_type(other)
            ))
        }
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };

    for (key, value) in supplied {
        // A client that sends an explicit `null` for an argument it chose not
        // to set is saying "absent", which is a legitimate thing to say.
        if value.is_null() {
            continue;
        }
        let Some(spec) = properties.get(key) else {
            let accepted: Vec<&str> = properties.keys().map(String::as_str).collect();
            let hint = match nearest(key, &accepted) {
                Some(near) => format!(" Did you mean `{near}`?"),
                None => String::new(),
            };
            let accepted = if accepted.is_empty() {
                "it takes no arguments".to_string()
            } else {
                format!("accepted: {}", accepted.join(", "))
            };
            return Err(format!(
                "Unknown argument `{key}` for {tool} — {accepted}.{hint} \
                 The call was rejected rather than run without it: an ignored filter \
                 returns an unfiltered result that looks like nothing is wrong."
            ));
        };
        check_type(tool, key, spec, value)?;
    }
    Ok(())
}

fn check_type(tool: &str, key: &str, spec: &Value, value: &Value) -> Result<(), String> {
    if let Some(allowed) = spec.get("enum").and_then(Value::as_array) {
        let names: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
        if !value.as_str().is_some_and(|v| names.contains(&v)) {
            return Err(format!(
                "Argument `{key}` for {tool} must be one of: {} — got {}.",
                names.join(", "),
                compact(value)
            ));
        }
        return Ok(());
    }

    match spec.get("type").and_then(Value::as_str) {
        Some("string") if !value.is_string() => Err(format!(
            "Argument `{key}` for {tool} must be a string, got {} ({}). \
             Rejected rather than dropped: the query would otherwise have run \
             without this filter and reported a clean result.",
            json_type(value),
            compact(value)
        )),
        // `num_arg` accepts a stringified number because some clients emit one;
        // validation has to accept exactly the same set, or a call this server
        // would have handled fine gets refused.
        Some("integer")
            if !(value.as_u64().is_some()
                || value.as_str().is_some_and(|s| s.parse::<u64>().is_ok())) =>
        {
            Err(format!(
                "Argument `{key}` for {tool} must be a non-negative integer, got {} ({}).",
                json_type(value),
                compact(value)
            ))
        }
        Some("boolean")
            if !(value.is_boolean()
                || value.as_str().is_some_and(|s| s.parse::<bool>().is_ok())) =>
        {
            Err(format!(
                "Argument `{key}` for {tool} must be true or false, got {} ({}).",
                json_type(value),
                compact(value)
            ))
        }
        _ => Ok(()),
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A short, quotable rendering of a rejected value for the error message.
fn compact(v: &Value) -> String {
    let raw = v.to_string();
    if raw.chars().count() <= 60 {
        raw
    } else {
        raw.chars().take(60).collect::<String>() + "…"
    }
}

/// Cheap "did you mean" for a misspelled argument — one edit away, or one name
/// contained in the other (`sources` for `source`, `time` for `timeout_ms`).
fn nearest<'a>(key: &str, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|c| c.contains(key) || key.contains(*c) || edit_distance(key, c) <= 2)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            row[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

fn request_protocol(params: &Value) -> Option<&str> {
    params
        .get("_meta")?
        .get("io.modelcontextprotocol/protocolVersion")?
        .as_str()
}

fn has_client_capabilities(params: &Value) -> bool {
    params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
        .is_some_and(Value::is_object)
}

fn stamp_modern_result(result: &mut Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    object
        .entry("resultType")
        .or_insert_with(|| json!("complete"));
    let meta = object.entry("_meta").or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            "io.modelcontextprotocol/serverInfo".into(),
            json!({ "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }),
        );
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

fn bool_arg(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| {
        v.as_bool()
            // Same leniency as `num_arg`: some clients stringify scalars.
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
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
    pub data: Option<Value>,
}

fn rpc_error_response(id: Value, error: RpcError) -> Value {
    let mut detail = json!({ "code": error.code, "message": error.message });
    if let Some(data) = error.data {
        detail["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": detail })
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
            "inputSchema": {
                "type": "object",
                "properties": {
                    "format": { "type": "string", "enum": ["text", "json"],
                                "description": FORMAT_DESC }
                },
                "additionalProperties": false
            }
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
                    "since": { "type": "string", "description":
                        "Ignore matches older than this ('30s', '5m', or RFC 3339). A wait \
                         also answers with a match already in the buffer, so without this — \
                         or 'cursor' — the previous build's 'compiled successfully' can \
                         satisfy the call instantly." },
                    "timeout_ms": { "type": "integer", "description":
                        "How long to wait before giving up (default 30000, max 120000)." },
                    "cursor": { "type": "integer", "description":
                        "Ignore anything at or before this sequence number. Take it from an \
                         earlier response so you match only events caused by your action." },
                    "require_new": { "type": "boolean", "description":
                        "Wait only for a line that arrives after this call starts, ignoring \
                         anything already in the buffer. Use it when you just triggered \
                         something and hold no earlier cursor — otherwise the previous \
                         build's matching line can satisfy the wait instantly." },
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
    async fn modern_discovery_advertises_current_protocol_and_identity() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": "discover", "method": "server/discover",
                "params": { "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }}
            }))
            .await
            .unwrap();
        assert_eq!(res["result"]["supportedVersions"][0], MODERN_PROTOCOL);
        assert_eq!(res["result"]["resultType"], "complete");
        assert_eq!(res["result"]["cacheScope"], "public");
        assert!(res["result"]["ttlMs"].as_u64().unwrap() > 0);
        assert_eq!(
            res["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            SERVER_NAME
        );
    }

    #[tokio::test]
    async fn modern_tool_list_is_cacheable_and_stamped() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list",
                "params": { "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }}
            }))
            .await
            .unwrap();
        assert_eq!(res["result"]["cacheScope"], "public");
        assert!(res["result"]["ttlMs"].as_u64().unwrap() > 0);
        assert_eq!(res["result"]["resultType"], "complete");
    }

    #[tokio::test]
    async fn modern_requests_reject_unsupported_versions() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 9, "method": "tools/list",
                "params": { "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2099-01-01",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }}
            }))
            .await
            .unwrap();
        assert_eq!(res["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(res["error"]["data"]["requested"], "2099-01-01");
        assert!(res["error"]["data"]["supported"].is_array());
    }

    #[tokio::test]
    async fn modern_requests_require_client_capabilities() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 10, "method": "tools/list",
                "params": { "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL
                }}
            }))
            .await
            .unwrap();
        assert_eq!(res["error"]["code"], MISSING_REQUIRED_CLIENT_CAPABILITY);
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

    // ── Argument validation ───────────────────────────────────────────────────
    //
    // Every test here asserts the same thing from a different angle: a call the
    // server cannot honour exactly must come back as a visible failure, never as
    // a successful-looking empty or unfiltered result. The daemon in these tests
    // is deliberately unreachable, so any answer that is *not* the "no daemon"
    // message proves validation ran first, before the network.

    async fn call(tool: &str, args: Value) -> Value {
        server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            }))
            .await
            .unwrap()
    }

    fn tool_error_text(res: &Value) -> String {
        assert!(res.get("error").is_none(), "must not be a protocol error");
        assert_eq!(
            res["result"]["isError"], true,
            "must be flagged as an error"
        );
        res["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn unknown_argument_is_rejected_not_run_unfiltered() {
        let res = call("get_recent_errors", json!({ "pattern": "timeout" })).await;
        let text = tool_error_text(&res);
        assert!(text.contains("`pattern`"), "must name the argument: {text}");
        assert!(text.contains("grep"), "must list what it accepts: {text}");
        assert!(
            !text.contains("No TailFlow daemon"),
            "must fail before the request is sent, not after: {text}"
        );
    }

    #[tokio::test]
    async fn misspelled_argument_suggests_the_real_one() {
        let text = tool_error_text(&call("search_logs", json!({ "sources": "api" })).await);
        assert!(text.contains("Did you mean `source`"), "got: {text}");
    }

    #[tokio::test]
    async fn argument_valid_on_another_tool_is_still_rejected() {
        // `timeout_ms` is a real TailFlow argument, just not one that means
        // anything to a tool that never blocks. Dropping it silently would let a
        // caller believe search_logs had waited for something.
        let text = tool_error_text(&call("search_logs", json!({ "timeout_ms": 5000 })).await);
        assert!(text.contains("`timeout_ms`"), "got: {text}");
        assert!(
            text.contains("accepted:"),
            "must list the real ones: {text}"
        );
    }

    #[tokio::test]
    async fn wait_accepts_since_to_bound_its_retrospective_half() {
        let text = tool_error_text(&call("wait_for_logs", json!({ "since": "5m" })).await);
        assert!(
            text.contains("No TailFlow daemon"),
            "`since` must reach the daemon, not be rejected: {text}"
        );
    }

    #[tokio::test]
    async fn wrong_typed_filter_is_rejected() {
        for bad in [json!(123), json!(["timeout", "refused"]), json!(true)] {
            let text = tool_error_text(&call("get_recent_errors", json!({ "grep": bad })).await);
            assert!(text.contains("must be a string"), "got: {text}");
            assert!(text.contains("`grep`"), "got: {text}");
        }
    }

    #[tokio::test]
    async fn non_numeric_integer_argument_is_rejected() {
        let text = tool_error_text(&call("search_logs", json!({ "limit": "many" })).await);
        assert!(text.contains("non-negative integer"), "got: {text}");
    }

    #[tokio::test]
    async fn negative_integer_argument_is_rejected() {
        // Would otherwise parse to None and silently become the default limit.
        let text = tool_error_text(&call("search_logs", json!({ "limit": -5 })).await);
        assert!(text.contains("`limit`"), "got: {text}");
    }

    #[tokio::test]
    async fn bad_enum_value_is_rejected() {
        let text = tool_error_text(&call("search_logs", json!({ "format": "yaml" })).await);
        assert!(text.contains("text, json"), "must list the choices: {text}");
    }

    #[tokio::test]
    async fn stringified_numbers_are_still_accepted() {
        // Some clients emit numbers as strings; validation must accept exactly
        // what `num_arg` accepts, or it refuses calls this server handles fine.
        let text = tool_error_text(&call("search_logs", json!({ "limit": "25" })).await);
        assert!(
            text.contains("No TailFlow daemon"),
            "should have reached the network: {text}"
        );
    }

    #[tokio::test]
    async fn explicit_null_means_absent_not_invalid() {
        let text = tool_error_text(
            &call("get_recent_errors", json!({ "grep": null, "since": null })).await,
        );
        assert!(
            text.contains("No TailFlow daemon"),
            "null is a client saying 'unset': {text}"
        );
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        let res = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "search_logs", "arguments": "grep=error" }
            }))
            .await
            .unwrap();
        let text = tool_error_text(&res);
        assert!(text.contains("must be a JSON object"), "got: {text}");
    }

    #[tokio::test]
    async fn omitted_arguments_are_valid() {
        let text = tool_error_text(&call("list_log_sources", json!({})).await);
        assert!(text.contains("No TailFlow daemon"), "got: {text}");
    }

    #[tokio::test]
    async fn list_log_sources_honours_format_rather_than_ignoring_it() {
        // It advertises `format`, so it has to act on it — an advertised argument
        // that does nothing is the same silent drop as an unknown one.
        let text = tool_error_text(&call("list_log_sources", json!({ "format": "json" })).await);
        assert!(text.contains("No TailFlow daemon"), "got: {text}");
        assert!(tool_schema("list_log_sources").unwrap()["properties"]["format"].is_object());
    }

    #[tokio::test]
    async fn every_advertised_tool_is_dispatchable() {
        // The schema lookup is the dispatch table; this pins them together so a
        // newly advertised tool cannot resolve to an "unknown tool" at call time.
        for tool in tool_definitions().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let res = call(name, json!({})).await;
            assert!(
                res.get("error").is_none(),
                "{name} is advertised but not dispatched"
            );
            assert_eq!(res["result"]["isError"], true, "{name}: daemon is down");
        }
    }

    #[tokio::test]
    async fn require_new_accepts_booleans_and_their_string_form() {
        for value in [json!(true), json!(false), json!("true")] {
            let text =
                tool_error_text(&call("wait_for_logs", json!({ "require_new": value })).await);
            assert!(text.contains("No TailFlow daemon"), "{value}: {text}");
        }
    }

    #[tokio::test]
    async fn require_new_rejects_a_non_boolean() {
        let text = tool_error_text(&call("wait_for_logs", json!({ "require_new": "yes" })).await);
        assert!(text.contains("must be true or false"), "got: {text}");
    }

    #[test]
    fn bool_arg_matches_the_leniency_of_num_arg() {
        let args = json!({ "a": true, "b": "false", "c": "yes", "d": 1 });
        assert_eq!(bool_arg(&args, "a"), Some(true));
        assert_eq!(bool_arg(&args, "b"), Some(false));
        assert_eq!(bool_arg(&args, "c"), None);
        assert_eq!(bool_arg(&args, "d"), None);
    }

    #[test]
    fn validation_covers_every_declared_property() {
        // A property advertised without a `type` would silently accept anything.
        for tool in tool_definitions().as_array().unwrap() {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["additionalProperties"], false);
            for (name, spec) in schema["properties"].as_object().unwrap() {
                assert!(
                    spec.get("type").and_then(Value::as_str).is_some(),
                    "{}.{name} has no type to validate against",
                    tool["name"]
                );
            }
        }
    }

    #[test]
    fn unknown_tool_has_no_schema() {
        assert!(tool_schema("drop_database").is_none());
        assert!(tool_schema("search_logs").is_some());
    }

    #[test]
    fn edit_distance_is_symmetric_and_zero_on_equal() {
        assert_eq!(edit_distance("source", "source"), 0);
        assert_eq!(edit_distance("sorce", "source"), 1);
        assert_eq!(
            edit_distance("grep", "limit"),
            edit_distance("limit", "grep")
        );
    }

    #[test]
    fn nearest_declines_when_nothing_is_close() {
        assert_eq!(nearest("pattern", &["grep", "level", "limit"]), None);
        assert_eq!(nearest("levl", &["grep", "level", "limit"]), Some("level"));
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
