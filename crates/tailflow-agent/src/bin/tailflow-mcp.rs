//! MCP stdio server exposing a running TailFlow daemon to an agent.
//!
//! Wire discipline: **stdout carries JSON-RPC and nothing else.** Every
//! diagnostic goes to stderr — a stray `println!` here corrupts the protocol
//! stream and the client drops the connection.

use anyhow::Result;
use clap::Parser;
use serde_json::Value;
use std::sync::Arc;
use tailflow_agent::{
    client::{DaemonClient, URL_ENV},
    mcp::{self, Server},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    version,
    name = "tailflow-mcp",
    about = "MCP server exposing your running local stack's logs to an AI agent",
    long_about = "Serves the Model Context Protocol over stdio, backed by a running \
                  tailflow-daemon.\n\n\
                  Register with Claude Code:\n    \
                  claude mcp add tailflow -- tailflow-mcp\n\n\
                  The daemon must be running separately (`tailflow-daemon` in your \
                  project root)."
)]
struct Cli {
    /// Daemon URL (default http://127.0.0.1:7878, or $TAILFLOW_URL)
    #[arg(long, value_name = "URL")]
    url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = DaemonClient::resolve(cli.url.as_deref());
    let url = client.url().to_string();

    // The daemon is allowed to be down at startup and come up later, so this
    // is a note, not a failure — the agent gets an actionable message from the
    // first tool call either way.
    eprintln!("tailflow-mcp: MCP server on stdio, reading logs from {url}");
    eprintln!("tailflow-mcp: override with --url or {URL_ENV}");

    let server = Arc::new(Server::new(client));

    // One writer task owns stdout, so concurrently-handled requests cannot
    // interleave their bytes mid-message.
    let (tx, mut rx) = mpsc::channel::<Value>(256);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let mut line = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("tailflow-mcp: failed to serialize response: {e}");
                    continue;
                }
            };
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break; // client closed the pipe
            }
            let _ = stdout.flush().await;
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        // Each request is handled concurrently: wait_for_logs can block for up
        // to two minutes, and must not stall the tool calls behind it.
        let server = server.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let response = match serde_json::from_str::<Value>(&line) {
                Err(e) => Some(mcp::parse_error(&e.to_string())),
                // Batching was removed from MCP in 2025-06-18; say so rather
                // than silently answering only the first element.
                Ok(v) if v.is_array() => Some(mcp::invalid_request(
                    "JSON-RPC batch requests are not supported",
                )),
                Ok(v) => server.handle(v).await,
            };
            if let Some(response) = response {
                let _ = tx.send(response).await;
            }
        });
    }

    drop(tx);
    let _ = writer.await;
    Ok(())
}
