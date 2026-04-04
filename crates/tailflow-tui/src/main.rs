mod app;
mod ui;

use anyhow::Result;
use clap::Parser;
use tailflow_core::{
    ingestion::{docker::DockerSource, file::FileSource, stdin::StdinSource, Source},
    new_bus,
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "tailflow", about = "Zero-config local log aggregator")]
struct Cli {
    /// Tail all running Docker containers
    #[arg(long)]
    docker: bool,

    /// Tail one or more log files
    #[arg(long = "file", value_name = "PATH")]
    files: Vec<std::path::PathBuf>,

    /// Label for stdin input (used when piping: cmd | tailflow)
    #[arg(long, value_name = "LABEL")]
    stdin: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let (tx, rx) = new_bus();
    let mut sources: Vec<Box<dyn Source>> = Vec::new();

    if cli.docker {
        let containers = DockerSource::discover().await?;
        if containers.is_empty() {
            eprintln!("tailflow: no running Docker containers found");
        }
        for src in containers {
            sources.push(Box::new(src));
        }
    }

    for path in cli.files {
        sources.push(Box::new(FileSource::new(path)));
    }

    if let Some(label) = cli.stdin {
        sources.push(Box::new(StdinSource::new(label)));
    } else if atty::isnt(atty::Stream::Stdin) {
        // Auto-detect piped stdin
        sources.push(Box::new(StdinSource::new("stdin")));
    }

    if sources.is_empty() {
        eprintln!("tailflow: no sources specified. Try: tailflow --docker");
        eprintln!("         or: tailflow --file /path/to/app.log");
        eprintln!("         or: npm run dev | tailflow");
        std::process::exit(1);
    }

    // Spawn each source
    for source in sources {
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = source.run(tx_clone).await {
                tracing::error!(err = %e, "source error");
            }
        });
    }
    drop(tx); // last sender held by spawned tasks

    let mut app = app::App::new(rx);
    app.run().await?;

    Ok(())
}
