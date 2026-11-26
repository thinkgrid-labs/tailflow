//! `tailflow-daemon sessions | session | agent ingest` — inspect agent
//! activity without a running daemon.
//!
//! These commands read the database file directly (like `storage status`)
//! and, unless `--no-ingest` is given, first move anything waiting in the
//! agent spool into it, so "run the agent, then look" works whether or not
//! a daemon was running. Ingesting here is safe next to a running daemon:
//! records are claimed by rename and events are deduplicated by id.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use clap::Args;
use std::sync::Arc;
use std::time::Duration;
use tailflow_core::agent::spool::Spool;
use tailflow_core::config::StorageConfig;
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};
use tailflow_storage::admin;
use tailflow_storage::ingest::SpoolIngestor;
use tailflow_storage::sessions::{SessionQuery, SessionView};
use tailflow_storage::{EventQuery, OpenOptions, Storage};

#[derive(Args)]
pub struct SessionsArgs {
    /// Only this agent (claude-code, codex)
    #[arg(long)]
    pub agent: Vec<String>,
    /// Only sessions whose project is this directory
    #[arg(long, value_name = "DIR")]
    pub project: Vec<std::path::PathBuf>,
    /// active, ended or unterminated
    #[arg(long)]
    pub status: Vec<String>,
    /// Activity since: RFC 3339 or 30m / 2h / 7d
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
    #[arg(long, default_value_t = 30)]
    pub limit: usize,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
    /// Do not ingest the agent spool first
    #[arg(long)]
    pub no_ingest: bool,
}

#[derive(Args)]
pub struct SessionArgs {
    /// Session id (`claude-code:<uuid>`) or a unique prefix of the agent's id
    pub id: String,
    /// Also print the session's events, oldest first
    #[arg(long)]
    pub events: bool,
    /// Events to print with --events
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub no_ingest: bool,
}

/// Move waiting spool records into the database. Returns a one-line summary
/// when anything happened.
pub fn ingest(cfg: &StorageConfig, privacy: &PrivacyConfig) -> Result<Option<String>> {
    let Some(spool) = Spool::default_location() else {
        return Ok(None);
    };
    if !crate::agent_ingest::is_default_store(&tailflow_storage::resolve_path(cfg)?) {
        // Agent events belong to the default per-user database only.
        return Ok(None);
    }
    let stats = spool.stats();
    if stats.pending_files == 0 && stats.overflow.count == 0 && stats.claimed_files == 0 {
        return Ok(None);
    }
    let policy = Arc::new(CapturePolicy::from_config(privacy).context("invalid [privacy]")?);
    let storage = Storage::open(
        cfg,
        OpenOptions {
            project_id: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            policy,
        },
    )?;
    let ing = SpoolIngestor::new(spool, storage.run_key().to_string());
    let report = ing.drain(&storage, Duration::from_secs(60));
    ing.release();
    storage.shutdown(Duration::from_secs(30))?;
    for e in &report.errors {
        eprintln!("tailflow: agent spool: {e}");
    }
    let mut line = format!(
        "ingested {} spooled hook call(s), {} event(s)",
        report.files, report.events_submitted
    );
    let lost = report.rejected + report.overflow + report.corrupt_files as u64;
    if lost > 0 {
        line.push_str(&format!(
            "; {lost} hook call(s) were lost and recorded as gaps"
        ));
    }
    if report.more {
        line.push_str("; more are waiting (run again)");
    }
    Ok(Some(line))
}

fn local(t: Option<DateTime<Utc>>) -> String {
    t.map_or_else(
        || "-".into(),
        |t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string(),
    )
}

fn duration(v: &SessionView) -> String {
    let (Some(a), Some(b)) = (
        v.started_at.or(v.first_event_at),
        v.ended_at.or(v.last_event_at),
    ) else {
        return "-".into();
    };
    let secs = (b - a).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn project_label(v: &SessionView) -> String {
    v.project_id
        .as_deref()
        .or(v.working_directory.as_deref())
        .map_or_else(
            || "-".into(),
            |p| {
                let home = std::env::var("HOME").unwrap_or_default();
                match p.strip_prefix(&home) {
                    Some(rest) if !home.is_empty() => format!("~{rest}"),
                    _ => p.to_string(),
                }
            },
        )
}

pub fn list(args: SessionsArgs, cfg: &StorageConfig, privacy: &PrivacyConfig) -> Result<()> {
    if !args.no_ingest {
        if let Some(line) = ingest(cfg, privacy)? {
            eprintln!("{line}");
        }
    }
    let path = tailflow_storage::resolve_path(cfg)?;
    if !path.exists() {
        bail!(
            "no database at {} yet. Install an integration (`tailflow agent install claude`) and run your agent.",
            path.display()
        );
    }
    let since = args
        .since
        .as_deref()
        .map(|s| {
            tailflow_core::query::parse_since(s).with_context(|| format!("invalid --since {s:?}"))
        })
        .transpose()?;
    let projects = args
        .project
        .iter()
        .map(|p| {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.clone())
                .display()
                .to_string()
        })
        .collect();
    let page = admin::sessions(
        &path,
        &SessionQuery {
            agents: args.agent,
            projects,
            statuses: args.status,
            since,
            limit: args.limit,
            ..SessionQuery::default()
        },
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }
    if page.sessions.is_empty() {
        println!("no agent sessions recorded");
        return Ok(());
    }
    println!(
        "{:<22} {:<12} {:<16} {:>6} {:>5} {:>5} {:>5}  {:<9} PROJECT",
        "SESSION", "STATUS", "STARTED", "DUR", "TOOLS", "FAIL", "CMDS", "HISTORY"
    );
    for v in &page.sessions {
        let id = format!("{}:{}", v.agent_name, short(&v.source_session_id));
        println!(
            "{:<22} {:<12} {:<16} {:>6} {:>5} {:>5} {:>5}  {:<9} {}",
            id,
            v.status,
            local(v.started_at.or(v.first_event_at)),
            duration(v),
            v.counts.tool_calls,
            // A failed shell call is one tool failure *and* one command
            // failure; count the incident once.
            v.counts.tool_failures,
            v.counts.commands,
            v.coverage.history,
            project_label(v)
        );
    }
    if page
        .sessions
        .iter()
        .any(|v| v.coverage.history != "complete")
    {
        println!("\nHISTORY other than `complete`: see `tailflow-daemon session <id>` for why.");
    }
    Ok(())
}

fn resolve_id(path: &std::path::Path, id: &str) -> Result<String> {
    if admin::session(path, id)?.is_some() {
        return Ok(id.to_string());
    }
    let page = admin::sessions(
        path,
        &SessionQuery {
            limit: 500,
            ..SessionQuery::default()
        },
    )?;
    let matches: Vec<&SessionView> = page
        .sessions
        .iter()
        .filter(|s| s.session_id.starts_with(id) || s.source_session_id.starts_with(id))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.session_id.clone()),
        [] => bail!("no session matches {id:?}"),
        many => bail!(
            "{id:?} matches {} sessions; use more characters ({})",
            many.len(),
            many.iter()
                .take(5)
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn show(args: SessionArgs, cfg: &StorageConfig, privacy: &PrivacyConfig) -> Result<()> {
    if !args.no_ingest {
        if let Some(line) = ingest(cfg, privacy)? {
            eprintln!("{line}");
        }
    }
    let path = tailflow_storage::resolve_path(cfg)?;
    if !path.exists() {
        bail!("no database at {}", path.display());
    }
    let key = resolve_id(&path, &args.id)?;
    let d = admin::session(&path, &key)?.with_context(|| format!("no session {key:?}"))?;
    let events = if args.events {
        let storage_events = open_reader_query(&path, &key, args.limit)?;
        Some(storage_events)
    } else {
        None
    };
    if args.json {
        let mut v = serde_json::to_value(&d)?;
        if let (Some(obj), Some(ev)) = (v.as_object_mut(), &events) {
            obj.insert("events".into(), serde_json::to_value(ev)?);
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let s = &d.session;
    println!("session    {}", s.session_id);
    println!(
        "agent      {}{}",
        s.agent_name,
        s.model
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default()
    );
    println!("project    {}", project_label(s));
    if let Some(b) = &s.git.branch {
        println!(
            "git        {b} @ {}{}",
            s.git.head_at_start.as_deref().map_or("?", short),
            match s.git.inferred_head_changed {
                Some(true) => format!(
                    " → {} (HEAD moved during the session; not necessarily by the agent)",
                    s.git.head_at_end.as_deref().map_or("?", short)
                ),
                _ => String::new(),
            }
        );
    }
    println!(
        "status     {}{}",
        s.status,
        s.end_reason
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default()
    );
    println!("started    {}", local(s.started_at));
    println!("ended      {}", local(s.ended_at));
    println!(
        "activity   {} → {}  ({})",
        local(s.first_event_at),
        local(s.last_event_at),
        duration(s)
    );
    println!(
        "counts     {} events, {} tool calls ({} failed), {} commands ({} failed), {} file events, {} subagents, {} turns{}",
        s.counts.events, s.counts.tool_calls, s.counts.tool_failures, s.counts.commands,
        s.counts.command_failures, s.counts.file_events, s.counts.subagents, s.counts.turns,
        s.counts.tokens_total.map(|t| format!(", {t} tokens")).unwrap_or_default()
    );
    println!(
        "history    {} {}",
        s.coverage.history,
        if s.coverage.reasons.is_empty() {
            String::new()
        } else {
            format!("({})", s.coverage.reasons.join(", "))
        }
    );
    for g in &s.coverage.gaps {
        println!(
            "  gap      {} ({})",
            g.reason,
            g.count
                .map_or_else(|| "unknown count".into(), |n| format!("{n}"))
        );
    }
    if !d.tools.is_empty() {
        println!(
            "tools      {}",
            d.tools
                .iter()
                .map(|t| format!("{} ×{}", t.name, t.count))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !d.programs.is_empty() {
        println!(
            "commands   {}",
            d.programs
                .iter()
                .map(|t| format!("{} ×{}", t.name, t.count))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !d.files.is_empty() {
        println!("files (agent-reported, not verified on disk):");
        for f in d.files.iter().take(15) {
            println!(
                "  {:<14} {}",
                f.last_event_type.trim_start_matches("file."),
                f.path
            );
        }
    }
    if !d.failures.is_empty() {
        println!("failures:");
        for f in &d.failures {
            println!(
                "  #{:<8} {}  {}",
                f.seq,
                local(f.timestamp),
                f.message.as_deref().unwrap_or(&f.event_type)
            );
        }
    }
    println!("limits     (what this integration cannot see)");
    for l in &s.coverage.integration_limits {
        println!("  {:<18} {:?}: {}", l.activity, l.level, l.note);
    }
    if let Some(events) = events {
        println!("\nevents (oldest first):");
        for e in &events.events {
            println!(
                "  #{:<8} {}  {:<24} {}",
                e.seq,
                e.event
                    .timestamp
                    .with_timezone(&Local)
                    .format("%H:%M:%S%.3f"),
                e.event.event_type,
                e.event.message.as_deref().unwrap_or("")
            );
        }
        if events.has_more {
            println!("  … more (raise --limit)");
        }
        if !events.completeness.complete {
            println!(
                "  history incomplete: {}",
                events.completeness.reasons.join(", ")
            );
        }
    }
    Ok(())
}

/// Events of one session through a short-lived read-only store handle.
fn open_reader_query(
    path: &std::path::Path,
    key: &str,
    limit: usize,
) -> Result<tailflow_storage::EventPage> {
    admin::query(
        path,
        &EventQuery {
            sessions: vec![key.to_string()],
            limit: limit.clamp(1, 1000),
            ..EventQuery::default()
        },
    )
    .map_err(Into::into)
}

pub fn ingest_now(cfg: &StorageConfig, privacy: &PrivacyConfig) -> Result<()> {
    match ingest(cfg, privacy)? {
        Some(line) => println!("{line}"),
        None => println!("the agent spool is empty"),
    }
    Ok(())
}
