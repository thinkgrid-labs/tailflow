mod agent_ingest;
mod api_v1;
mod routes;
mod session_cli;
mod state;
mod storage_cli;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use std::{net::SocketAddr, path::PathBuf};
use tailflow_core::{
    config::Config,
    ingestion::{docker::DockerAllSource, file::FileSource, stdin::StdinSource, Source},
    new_bus,
    privacy::CapturePolicy,
    processor::Filter,
};
use tailflow_storage::{OpenOptions, Storage};
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
    #[command(subcommand)]
    command: Option<Command>,

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
    #[arg(long, value_name = "PATH", global = true)]
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

    /// Memory cap for that buffer, in MiB of retained log text. Ordinary
    /// lines reach the record limit first; this bounds the buffer when lines
    /// are large. Older records stay in durable history.
    #[arg(long, value_name = "MIB", default_value_t = 16,
          value_parser = clap::value_parser!(u32).range(1..=4096))]
    buffer_mb: u32,

    /// Keep history in memory only (overrides `[storage] enabled`)
    #[arg(long)]
    no_storage: bool,

    /// SQLite database for durable history (overrides `[storage] path`)
    #[arg(long, value_name = "PATH", global = true)]
    db: Option<PathBuf>,

    /// Run even without log sources, to record coding-agent activity only.
    /// (Implied when an agent integration is installed.)
    #[arg(long, conflicts_with = "no_agents")]
    agents: bool,

    /// Do not ingest coding-agent hook events (they stay in the spool for
    /// another daemon or `tailflow-daemon agent ingest`)
    #[arg(long)]
    no_agents: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect or maintain durable history. Works whether or not a daemon is
    /// running; SQLite serializes access.
    Storage {
        #[command(subcommand)]
        action: storage_cli::StorageAction,
    },
    /// List recorded coding-agent sessions (works without a running daemon)
    Sessions(session_cli::SessionsArgs),
    /// Show one coding-agent session: lifecycle, tools, commands, files,
    /// failures, and what the integration cannot see
    Session(session_cli::SessionArgs),
    /// Agent capture maintenance
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Move spooled agent hook events into the database now
    Ingest,
}

/// The loaded config plus the directory it came from (the project root).
fn load_config(cli_config: Option<&std::path::Path>) -> Result<(Option<Config>, PathBuf)> {
    let cwd = std::env::current_dir()?;
    match cli_config {
        Some(path) => {
            let root = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| cwd.clone(), std::path::Path::to_path_buf);
            Ok((Some(Config::load(path)?), root))
        }
        None => {
            let mut dir = cwd.clone();
            loop {
                if dir.join("tailflow.toml").exists() {
                    let cfg = Config::load(&dir.join("tailflow.toml"))?;
                    return Ok((Some(cfg), dir));
                }
                if !dir.pop() {
                    return Ok((None, cwd));
                }
            }
        }
    }
}

#[cfg(unix)]
async fn terminate_signal() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

/// Windows has no SIGTERM. Besides Ctrl+C (handled by the caller), stop
/// gracefully on Ctrl+Break — how a supervisor stops a console process
/// started in its own process group. Console close is best effort: Windows
/// ends the process about 5 s after delivering it, which a busy shutdown
/// can exceed (the loss is then recorded at the next start, as for any
/// unclean stop). Logoff and shutdown events reach services only; they are
/// listened for in case the daemon is run as one.
#[cfg(windows)]
async fn terminate_signal() {
    use tokio::signal::windows;
    // A listener that cannot be registered simply never fires.
    macro_rules! event {
        ($listener:expr) => {
            async {
                match $listener {
                    Ok(mut s) => {
                        s.recv().await;
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            }
        };
    }
    tokio::select! {
        () = event!(windows::ctrl_break()) => {}
        () = event!(windows::ctrl_close()) => {}
        () = event!(windows::ctrl_shutdown()) => {}
        () = event!(windows::ctrl_logoff()) => {}
    }
}

#[cfg(not(any(unix, windows)))]
async fn terminate_signal() {
    std::future::pending::<()>().await;
}

/// Async worker threads when `TOKIO_WORKER_THREADS` is not set.
///
/// Measured in M1.5 (4C/8T host): 2 workers instead of Tokio's default of one
/// per logical CPU cut daemon CPU 23 % at 1,000 ev/s, 39 % with four SSE
/// clients and 14 % in a mixed ingest + query workload, with equal or lower
/// query and SSE latency. The work per event is small; most of the cost was
/// waking idle workers on other cores. SQLite queries and writes run on
/// separate threads (blocking pool, writer thread), so this does not limit
/// query concurrency. One worker was measured too: worse SSE tail latency.
const DEFAULT_WORKERS: usize = 2;

fn main() -> Result<()> {
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    // An explicit TOKIO_WORKER_THREADS still wins (Tokio reads it when no
    // count is set on the builder).
    if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        runtime.worker_threads(cpus.clamp(1, DEFAULT_WORKERS));
    }
    runtime.build()?.block_on(run())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let (config, project_root) = load_config(cli.config.as_deref())?;

    let mut storage_cfg = config
        .as_ref()
        .map(|c| c.storage.clone())
        .unwrap_or_default();
    let privacy_cfg = config
        .as_ref()
        .map(|c| c.privacy.clone())
        .unwrap_or_default();
    if let Some(db) = &cli.db {
        storage_cfg.path = Some(db.clone());
    }

    match cli.command {
        Some(Command::Storage { action }) => return storage_cli::run(action, &storage_cfg),
        Some(Command::Sessions(args)) => {
            return session_cli::list(args, &storage_cfg, &privacy_cfg)
        }
        Some(Command::Session(args)) => return session_cli::show(args, &storage_cfg, &privacy_cfg),
        Some(Command::Agent {
            action: AgentAction::Ingest,
        }) => return session_cli::ingest_now(&storage_cfg, &privacy_cfg),
        None => {}
    }

    if cli.no_storage {
        storage_cfg.enabled = false;
    }
    // Invalid storage or privacy settings are fatal: a typo must not silently
    // turn redaction off or retention into something else.
    storage_cfg.validate()?;
    let policy = Arc::new(
        CapturePolicy::from_config(&privacy_cfg).context("invalid [privacy] configuration")?,
    );

    let (tx, rx) = new_bus();
    let mut sources: Vec<Box<dyn Source>> = Vec::new();

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
    } else if !std::io::stdin().is_terminal() {
        sources.push(Box::new(StdinSource::new("stdin")));
    }

    let integrations_installed = tailflow_core::paths::default_agents_dir()
        .and_then(|d| tailflow_core::agent::install::IntegrationState::load(&d).ok())
        .is_some_and(|s| !s.integrations.is_empty());
    let agents_wanted = !cli.no_agents && storage_cfg.enabled;
    if sources.is_empty() && !(agents_wanted && (cli.agents || integrations_installed)) {
        eprintln!(
            "tailflow-daemon: no sources. Add a tailflow.toml or use --docker / --file \
             (or --agents to record coding-agent activity only)."
        );
        std::process::exit(1);
    }

    let project_id = std::fs::canonicalize(&project_root)
        .unwrap_or(project_root)
        .display()
        .to_string();
    let storage_slot = open_storage(&storage_cfg, policy.clone(), project_id).await;
    let run_key = storage_slot
        .storage()
        .map_or_else(|| "memory".to_string(), |s| s.run_key().to_string());
    let storage_handle = storage_slot.storage().cloned();
    let storage_line = describe_storage(&storage_slot, &storage_cfg);

    // Apply global CLI filters before records enter the ring buffer / SSE bus.
    let filter = {
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
        filter
    };
    let shutdown = CancellationToken::new();
    let (agent_status, agent_handle) = match (&storage_handle, agents_wanted) {
        (Some(storage), true) if !agent_ingest::is_default_store(storage.path()) => (
            agent_ingest::disabled(
                "custom database; agent events are ingested only into the default per-user database",
            ),
            None,
        ),
        (Some(storage), true) => match tailflow_core::agent::spool::Spool::default_location() {
            Some(spool) => {
                let status = std::sync::Arc::new(std::sync::Mutex::new(
                    agent_ingest::IngestStatus::default(),
                ));
                match agent_ingest::spawn(storage.clone(), spool, &shutdown, status.clone()) {
                    Ok(h) => (status, Some(h)),
                    Err(e) => {
                        eprintln!("tailflow-daemon: agent capture unavailable: {e}");
                        (
                            agent_ingest::disabled(&format!("spool unavailable: {e}")),
                            None,
                        )
                    }
                }
            }
            None => (
                agent_ingest::disabled("no home directory for the agent spool"),
                None,
            ),
        },
        (None, true) => (
            agent_ingest::disabled("durable storage is not available"),
            None,
        ),
        (_, false) => (
            agent_ingest::disabled(if cli.no_agents {
                "--no-agents"
            } else {
                "storage disabled"
            }),
            None,
        ),
    };
    let agents_line = if agent_handle.is_some() {
        format!(
            "recording coding-agent hooks{}",
            if integrations_installed {
                ""
            } else {
                " (none installed: `tailflow agent install claude|codex`)"
            }
        )
    } else {
        agent_status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .disabled_reason
            .clone()
            .map_or_else(|| "off".into(), |r| format!("off ({r})"))
    };
    let (shared, fanout) = state::AppState::with_pipeline(
        rx,
        state::RingLimits {
            records: cli.buffer.max(1),
            bytes: usize::try_from(cli.buffer_mb)
                .unwrap_or(usize::MAX)
                .saturating_mul(1024 * 1024),
        },
        state::Pipeline {
            filter,
            policy: policy.clone(),
            storage: storage_slot,
            run_key,
            shutdown: shutdown.clone(),
            agent_ingest: agent_status,
        },
    );

    let mut source_tasks = Vec::new();
    for source in sources {
        let name = source.name().to_string();
        shared.register_source_kind(&name, source.kind());
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
                    let _no_receivers = tx_clone.send(tailflow_core::LogRecord {
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "tailflow-daemon listening");
    eprintln!("TailFlow v{} · daemon", env!("CARGO_PKG_VERSION"));
    eprintln!("  dashboard   http://{addr}");
    eprintln!("  SSE stream  http://{addr}/events");
    eprintln!("  agent API   http://{addr}/api/errors");
    eprintln!("  history     {storage_line}");
    eprintln!("  privacy     capture = {}", policy.mode.as_str());
    eprintln!("  agents      {agents_line}");

    let shutdown_signal = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                () = terminate_signal() => {}
            }
            shutdown_signal.cancel();
        })
        .await?;
    shutdown.cancel();
    // Agent ingestion stops first: its in-flight pass flushes into a storage
    // that is still running, and unfinished claims go back to the spool.
    if let Some(h) = agent_handle {
        // Bounded: a pass stops after its current event; anything still
        // claimed is replayed later (deduplicated), never lost.
        let stopping = tokio::task::spawn_blocking(move || h.stop());
        match tokio::time::timeout(Duration::from_secs(10), stopping).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => tracing::warn!("agent ingest shutdown task failed"),
            Err(_) => tracing::warn!("agent ingest did not stop within 10 s; continuing"),
        }
    }
    if tokio::time::timeout(Duration::from_secs(5), async move {
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
    // The fan-out ends once every source sender is gone; let it hand its last
    // records to storage before the writer drains.
    let fanout_abort = fanout.abort_handle();
    if tokio::time::timeout(Duration::from_secs(3), fanout)
        .await
        .is_err()
    {
        // Stop it so nothing is submitted after storage drains; anything it
        // still held is counted by storage's shutdown as late loss.
        tracing::warn!("timed out waiting for the ingestion fan-out; stopping it");
        fanout_abort.abort();
    }
    if let Some(storage) = storage_handle {
        match tokio::task::spawn_blocking(move || storage.shutdown(Duration::from_secs(15))).await {
            Ok(Ok(())) => info!("storage closed cleanly"),
            Ok(Err(e)) => {
                eprintln!("tailflow-daemon: storage did not close cleanly ({e}); the next start will record a gap");
            }
            Err(e) => tracing::warn!(err = ?e, "storage shutdown task failed"),
        }
    }

    Ok(())
}

async fn open_storage(
    cfg: &tailflow_core::config::StorageConfig,
    policy: Arc<CapturePolicy>,
    project_id: String,
) -> state::StorageSlot {
    if !cfg.enabled {
        return state::StorageSlot::Disabled;
    }
    let cfg = cfg.clone();
    let opened = tokio::task::spawn_blocking(move || {
        Storage::open(
            &cfg,
            OpenOptions {
                project_id: Some(project_id),
                version: env!("CARGO_PKG_VERSION").to_string(),
                policy,
            },
        )
    })
    .await;
    match opened {
        Ok(Ok(storage)) => {
            for run in storage.recovered_runs() {
                eprintln!(
                    "tailflow-daemon: previous run {} ended without a clean shutdown; \
                     recorded a history gap after seq {}",
                    run.run_id, run.last_seq
                );
            }
            state::StorageSlot::Enabled(storage)
        }
        Ok(Err(e)) => {
            eprintln!("tailflow-daemon: durable history unavailable: {e}");
            eprintln!("tailflow-daemon: continuing memory-only; live logs are unaffected");
            state::StorageSlot::Unavailable {
                kind: e.kind(),
                message: e.to_string(),
            }
        }
        Err(e) => state::StorageSlot::Unavailable {
            kind: "internal",
            message: format!("storage open task failed: {e}"),
        },
    }
}

fn describe_storage(
    slot: &state::StorageSlot,
    cfg: &tailflow_core::config::StorageConfig,
) -> String {
    match slot {
        state::StorageSlot::Enabled(s) => format!(
            "{} ({}, {} MB cap)",
            s.path().display(),
            cfg.retention_days
                .days()
                .map_or_else(|| "unlimited age".to_string(), |d| format!("{d}d")),
            cfg.max_size_mb
        ),
        state::StorageSlot::Disabled => "memory only (storage disabled)".into(),
        state::StorageSlot::Unavailable { kind, .. } => {
            format!("UNAVAILABLE ({kind}) — memory only")
        }
    }
}
