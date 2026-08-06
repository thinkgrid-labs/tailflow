//! Agent-facing access to a running TailFlow daemon.
//!
//! Ships two binaries over one implementation:
//!
//! - `tailflow-mcp` — an MCP stdio server, for agents that speak MCP.
//! - `tailflow-logs` — one-shot CLI queries, for agents that only have a shell.
//!
//! Neither depends on `tailflow-core`. They are HTTP clients of a daemon that
//! is already running, so they carry none of the ingestion dependency tree and
//! stay small enough to ship over `npx`.

pub mod client;
pub mod mcp;
pub mod ops;
pub mod render;
