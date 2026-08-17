mod routes;
mod state;

use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tailflow_core::{
    config::Config,
    ingestion::{docker::DockerAllSource, file::FileSource, stdin::StdinSource, Source},
    new_bus,
    processor::{filtered_bus, Filter},
};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    name = "tailflow-daemon",
    about = "TailFlow daemon — collects your stack's logs and serves them over HTTP"
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

    /// Records retained for retrospective queries (/api/query, /api/errors)
    #[arg(long, value_name = "N", default_value_t = tailflow_core::query::DEFAULT_CAPACITY)]
    buffer: usize,
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
        eprintln!("tailflow-daemon: no sources. Add a tailflow.toml or use --docker / --file.");
        std::process::exit(1);
    }

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
    let shared = state::AppState::new(rx, cli.buffer.max(1));

    let shutdown = CancellationToken::new();
    let mut source_tasks = Vec::new();
    for source in sources {
        let name = source.name().to_string();
        shared.register_source(&name);
        let tx_clone = tx.clone();
        let shared = shared.clone();
        let source_shutdown = shutdown.child_token();
        source_tasks.push(tokio::spawn(async move {
            shared.mark_source_running(&name);
            match source.run(tx_clone.clone(), source_shutdown).await {
                Ok(()) => shared.mark_source_exited(&name, None),
                Err(e) => {
                    tracing::error!(source = %name, err = %e, "source error");
                    shared.mark_source_exited(&name, Some(e.to_string()));
                    let _ = tx_clone.send(tailflow_core::LogRecord {
                        timestamp: chrono::Utc::now(),
                        source: name,
                        level: tailflow_core::LogLevel::Error,
                        payload: format!("[tailflow] source failed: {e}"),
                    });
                }
            }
        }));
    }
    drop(tx);
    let source_aborts: Vec<_> = source_tasks
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect();
    let app = routes::router(shared);

    let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
    info!(%addr, "tailflow-daemon listening");
    eprintln!("tailflow-daemon: dashboard   http://{addr}");
    eprintln!("                 SSE stream  http://{addr}/events");
    eprintln!("                 agent API   http://{addr}/api/errors");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let shutdown_signal = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_signal.cancel();
        })
        .await?;
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

    Ok(())
}
