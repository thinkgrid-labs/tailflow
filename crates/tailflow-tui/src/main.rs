mod agent_cli;
mod app;
mod init;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tailflow_core::{
    config::Config,
    ingestion::{docker::DockerAllSource, file::FileSource, stdin::StdinSource, Source},
    new_bus,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    name = "tailflow",
    about = "Runtime verification for coding agents and developers"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to tailflow.toml (auto-discovered if omitted)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Tail all running Docker containers
    #[arg(long)]
    docker: bool,

    /// Tail one or more log files
    #[arg(long = "file", value_name = "PATH")]
    files: Vec<PathBuf>,

    /// Label for stdin input (used when piping: cmd | tailflow)
    #[arg(long, value_name = "LABEL")]
    stdin: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Detect local sources and create tailflow.toml
    Init(init::InitArgs),
    /// Coding-agent integrations: install, uninstall, status, list
    Agent {
        #[command(subcommand)]
        command: agent_cli::AgentCommand,
    },
    /// List recorded coding-agent sessions (runs `tailflow-daemon sessions`)
    #[command(disable_help_flag = true)]
    Sessions {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show one coding-agent session (runs `tailflow-daemon session`)
    #[command(disable_help_flag = true)]
    Session {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Run `tailflow-daemon <sub> <args>`: the session commands live in the
/// daemon binary, which links the SQLite store; this binary does not, so
/// that the agent hook bridge (which shares it) stays small.
fn forward_to_daemon(sub: &str, args: &[String]) -> Result<()> {
    let name = if cfg!(windows) {
        "tailflow-daemon.exe"
    } else {
        "tailflow-daemon"
    };
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|e| std::fs::canonicalize(e).ok())
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file());
    let program = sibling.unwrap_or_else(|| PathBuf::from(name));
    let status = std::process::Command::new(&program)
        .arg(sub)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {}: {e}", program.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Init(args)) => return init::run(args),
        // The hook path must not start a runtime or threads: it runs once
        // per agent tool call.
        Some(Command::Agent { command }) => return agent_cli::run(command),
        Some(Command::Sessions { args }) => return forward_to_daemon("sessions", &args),
        Some(Command::Session { args }) => return forward_to_daemon("session", &args),
        None => {}
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_tui(cli))
}

async fn run_tui(cli: Cli) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let (tx, rx) = new_bus();
    let mut sources: Vec<Box<dyn Source>> = Vec::new();

    // Load config (file flag → auto-discover → none)
    let config = if let Some(path) = cli.config.as_deref() {
        Some(Config::load(path)?)
    } else {
        Config::find_and_load(&std::env::current_dir()?)?
    };

    if let Some(cfg) = config {
        sources.extend(cfg.into_sources().await?);
    }

    // CLI flags are additive on top of config
    if cli.docker {
        sources.push(Box::new(DockerAllSource::new()));
    }

    for path in cli.files {
        sources.push(Box::new(FileSource::new(path)));
    }

    if let Some(label) = cli.stdin {
        sources.push(Box::new(StdinSource::new(label)));
    } else if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        sources.push(Box::new(StdinSource::new("stdin")));
    }

    if sources.is_empty() {
        eprintln!("tailflow: no sources. Add a tailflow.toml or use --docker / --file.");
        eprintln!("  Example: tailflow --docker");
        eprintln!("           npm run dev | tailflow");
        std::process::exit(1);
    }

    let shutdown = CancellationToken::new();
    let mut source_tasks = Vec::new();
    for source in sources {
        let tx_clone = tx.clone();
        let source_shutdown = shutdown.child_token();
        source_tasks.push(tokio::spawn(async move {
            if let Err(e) = source.run(tx_clone, source_shutdown).await {
                tracing::error!(err = %e, "source error");
            }
        }));
    }
    drop(tx);
    let source_aborts: Vec<_> = source_tasks
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect();

    let mut app = app::App::new(rx);
    let result = app.run().await;
    shutdown.cancel();
    if tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        for task in source_tasks {
            if let Err(error) = task.await {
                tracing::warn!(err = ?error, "source task did not shut down cleanly");
            }
        }
    })
    .await
    .is_err()
    {
        tracing::warn!("timed out waiting for source shutdown");
        for abort in source_aborts {
            abort.abort();
        }
    }
    result?;

    Ok(())
}
