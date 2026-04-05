mod routes;
mod state;

use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tailflow_core::{
    config::Config,
    ingestion::{docker::DockerSource, file::FileSource, stdin::StdinSource, Source},
    new_bus,
    processor::{filtered_bus, Filter},
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "tailflow-daemon",
    about = "TailFlow SSE daemon — stream logs over HTTP"
)]
struct Cli {
    /// Port to listen on
    #[arg(long, default_value = "7878")]
    port: u16,

    /// Tail all running Docker containers
    #[arg(long)]
    docker: bool,

    /// Tail log files
    #[arg(long = "file", value_name = "PATH")]
    files: Vec<PathBuf>,

    /// Label for piped stdin
    #[arg(long, value_name = "LABEL")]
    stdin: Option<String>,

    /// Path to tailflow.toml (auto-discovered if omitted)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Only stream records whose payload matches this regex
    #[arg(long, value_name = "REGEX")]
    grep: Option<String>,

    /// Only stream records from sources whose name contains this string
    #[arg(long, value_name = "NAME")]
    source: Option<String>,
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

    // Config file takes priority; CLI flags are additive
    let cfg_path = cli.config.as_deref();

    let config = if let Some(path) = cfg_path {
        Some(Config::load(path)?)
    } else {
        Config::find_and_load(&std::env::current_dir()?)?
    };

    if let Some(cfg) = config {
        info!("loaded tailflow.toml");
        sources.extend(cfg.into_sources().await?);
    }

    // CLI overrides / additions
    if cli.docker {
        for c in DockerSource::discover().await? {
            sources.push(Box::new(c));
        }
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
        eprintln!("tailflow-daemon: no sources. Add a tailflow.toml or use --docker / --file.");
        std::process::exit(1);
    }

    for source in sources {
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = source.run(tx_clone).await {
                tracing::error!(err = %e, "source error");
            }
        });
    }
    drop(tx);

    // Apply global CLI filters before records enter the ring buffer / SSE bus.
    let rx = {
        let mut filter = match cli.grep.as_deref() {
            Some(pat) => Filter::regex(pat).unwrap_or_else(|e| {
                eprintln!("tailflow-daemon: invalid --grep regex ({e}), filter ignored");
                Filter::none()
            }),
            None => Filter::none(),
        };
        if let Some(src) = cli.source {
            filter = filter.with_source(src);
        }
        filtered_bus(rx, filter)
    };

    let shared = state::AppState::new(rx);
    let app = routes::router(shared);

    let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
    info!(%addr, "tailflow-daemon listening");
    eprintln!("tailflow-daemon: SSE stream at http://{addr}/events");
    eprintln!("                 Recent logs at http://{addr}/api/records");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
