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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Init(args)) = cli.command {
        return init::run(args);
    }

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
    } else if atty::isnt(atty::Stream::Stdin) {
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
