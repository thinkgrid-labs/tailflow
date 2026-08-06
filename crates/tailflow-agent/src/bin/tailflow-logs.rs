//! One-shot queries against a running TailFlow daemon.
//!
//! The shell-shaped door to the same data `tailflow-mcp` serves, for agents
//! and humans that have a terminal rather than an MCP client. Exit codes are
//! meaningful so a caller can branch without parsing the output.

use clap::{Parser, Subcommand};
use serde_json::Value;
use tailflow_agent::{
    client::{ClientError, DaemonClient},
    ops::{self, ErrorsArgs, SearchArgs, WaitArgs},
    render,
};

/// Exit codes, so a script can branch on the outcome.
mod exit {
    /// Success — and for `wait`, the condition occurred.
    pub const OK: i32 = 0;
    /// The request was rejected (bad regex, bad level, bad duration).
    pub const BAD_REQUEST: i32 = 1;
    /// `wait` reached its timeout without matching.
    pub const NO_MATCH: i32 = 2;
    /// No daemon is listening.
    pub const NO_DAEMON: i32 = 3;
}

#[derive(Parser)]
#[command(
    name = "tailflow-logs",
    about = "Query a running TailFlow daemon from the shell",
    long_about = "One-shot reads of your running local stack, for agents and scripts.\n\n\
                  Requires a running daemon (`tailflow-daemon` in your project root).\n\n\
                  Exit codes: 0 ok / matched, 1 bad request, 2 wait timed out, \
                  3 no daemon.",
    version
)]
struct Cli {
    /// Daemon URL (default http://127.0.0.1:7878, or $TAILFLOW_URL)
    #[arg(long, global = true, value_name = "URL")]
    url: Option<String>,

    /// Print the raw JSON response instead of compact text
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Distinct failures, deduplicated, with stack traces
    Errors {
        /// How far back: 30s, 5m, 2h, 1d, or an RFC 3339 timestamp
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// Only this service (substring match)
        #[arg(long, value_name = "NAME")]
        source: Option<String>,
        /// Minimum severity (default: error)
        #[arg(long, value_name = "LEVEL")]
        level: Option<String>,
        /// Additional regex the line must match
        #[arg(long, value_name = "REGEX")]
        grep: Option<String>,
        /// Maximum distinct groups
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Trailing stack-trace lines per group (0 to omit)
        #[arg(long, value_name = "N")]
        context: Option<usize>,
    },

    /// Individual log lines matching a filter
    Search {
        /// Regex matched against the line body
        #[arg(value_name = "REGEX")]
        grep: Option<String>,
        #[arg(long, value_name = "NAME")]
        source: Option<String>,
        #[arg(long, value_name = "LEVEL")]
        level: Option<String>,
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Only lines newer than this sequence number
        #[arg(long, value_name = "SEQ")]
        cursor: Option<u64>,
    },

    /// What is running, and how noisy it is
    Sources,

    /// Block until a matching line appears
    Wait {
        /// Regex the line must match
        #[arg(long, value_name = "REGEX")]
        grep: Option<String>,
        #[arg(long, value_name = "NAME")]
        source: Option<String>,
        /// Only wake on lines at or above this severity
        #[arg(long, value_name = "LEVEL")]
        level: Option<String>,
        /// Give up after this many milliseconds (max 120000)
        #[arg(long, value_name = "MS", default_value_t = 30_000)]
        timeout_ms: u64,
        /// Ignore anything at or before this sequence number
        #[arg(long, value_name = "SEQ")]
        cursor: Option<u64>,
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    /// Check whether a daemon is reachable
    Status,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = DaemonClient::resolve(cli.url.as_deref());
    std::process::exit(run(&cli, &client).await);
}

async fn run(cli: &Cli, client: &DaemonClient) -> i32 {
    match &cli.command {
        Command::Errors {
            since,
            source,
            level,
            grep,
            limit,
            context,
        } => {
            let args = ErrorsArgs {
                grep: grep.clone(),
                source: source.clone(),
                level: level.clone(),
                since: since.clone(),
                limit: *limit,
                context_lines: *context,
            };
            emit(ops::errors(client, &args).await, cli.json, render::errors)
        }

        Command::Search {
            grep,
            source,
            level,
            since,
            limit,
            cursor,
        } => {
            let args = SearchArgs {
                grep: grep.clone(),
                source: source.clone(),
                level: level.clone(),
                since: since.clone(),
                limit: *limit,
                cursor: *cursor,
            };
            emit(ops::search(client, &args).await, cli.json, render::records)
        }

        Command::Sources => emit(ops::sources(client).await, cli.json, render::sources),

        Command::Wait {
            grep,
            source,
            level,
            timeout_ms,
            cursor,
            limit,
        } => {
            let args = WaitArgs {
                grep: grep.clone(),
                source: source.clone(),
                level: level.clone(),
                timeout_ms: Some(*timeout_ms),
                cursor: *cursor,
                limit: *limit,
            };
            match ops::wait(client, &args).await {
                Ok(v) => {
                    print(&v, cli.json, render::wait);
                    // Distinguish "it happened" from "it never happened" in the
                    // exit code, so a script can branch without reading stdout.
                    if v.get("matched").and_then(Value::as_bool) == Some(true) {
                        exit::OK
                    } else {
                        exit::NO_MATCH
                    }
                }
                Err(e) => fail(e),
            }
        }

        Command::Status => match ops::health(client).await {
            Ok(v) => {
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                } else {
                    println!(
                        "daemon ok at {} · version {} · {} records buffered · cursor {}",
                        client.url(),
                        v.get("version").and_then(Value::as_str).unwrap_or("?"),
                        v.get("buffered").and_then(Value::as_u64).unwrap_or(0),
                        v.get("cursor").and_then(Value::as_u64).unwrap_or(0),
                    );
                }
                exit::OK
            }
            Err(e) => fail(e),
        },
    }
}

fn emit(result: Result<Value, ClientError>, json: bool, renderer: fn(&Value) -> String) -> i32 {
    match result {
        Ok(v) => {
            print(&v, json, renderer);
            exit::OK
        }
        Err(e) => fail(e),
    }
}

fn print(v: &Value, json: bool, renderer: fn(&Value) -> String) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        print!("{}", renderer(v));
    }
}

fn fail(e: ClientError) -> i32 {
    eprintln!("tailflow-logs: {e}");
    match e {
        ClientError::NotRunning { .. } => exit::NO_DAEMON,
        ClientError::Http { status: 400, .. } => exit::BAD_REQUEST,
        _ => exit::BAD_REQUEST,
    }
}
