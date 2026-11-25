//! `tailflow agent …` — opt-in coding-agent integrations.
//!
//! `install` / `uninstall` edit the agent's own hook configuration (see
//! `tailflow_core::agent::install` for the safety rules); `status` reports
//! what is installed and whether events are flowing; `hook` is what the agent
//! runs, and must stay silent and fast.

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use std::io::Write;
use std::path::{Path, PathBuf};
use tailflow_core::agent::install::{self, ClaudeScope, IntegrationRecord, IntegrationState};
use tailflow_core::agent::spool::Spool;
use tailflow_core::agent::{capabilities, hook, AgentKind, CoverageLevel};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Scope {
    /// Every project for this user (default)
    User,
    /// This project, shared with the team (`.claude/settings.json`)
    Project,
    /// This project, only for you (`.claude/settings.local.json`)
    Local,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Install TailFlow's observe-only hooks into an agent's configuration
    Install {
        /// claude (Claude Code) or codex
        agent: String,
        /// Claude Code settings scope
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        /// Project directory for --scope project/local (default: current dir)
        #[arg(long, value_name = "DIR")]
        project_dir: Option<PathBuf>,
        /// Edit this file instead of the agent's default configuration
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Codex version to target (default: detected from `codex --version`)
        #[arg(long, value_name = "X.Y.Z")]
        codex_version: Option<String>,
        /// Show the resulting configuration without writing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove TailFlow's hooks (and only TailFlow's) from an agent's configuration
    Uninstall {
        agent: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        #[arg(long, value_name = "DIR")]
        project_dir: Option<PathBuf>,
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Show installed integrations, policy blockers and the event spool
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List supported agents and what each integration can and cannot see
    List {
        #[arg(long)]
        json: bool,
    },
    /// Hook entry point run by the agent (reads the event on stdin)
    #[command(hide = true)]
    Hook { agent: String },
}

fn parse_agent(s: &str) -> Result<AgentKind> {
    AgentKind::parse(s).with_context(|| format!("unknown agent {s:?} (supported: claude, codex)"))
}

fn agents_dir() -> Result<PathBuf> {
    tailflow_core::paths::default_agents_dir()
        .context("no home directory to keep TailFlow's agent state in (set TAILFLOW_DATA_DIR)")
}

fn config_path(
    kind: AgentKind,
    scope: Scope,
    project_dir: Option<PathBuf>,
    config: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(c) = config {
        return Ok(c);
    }
    match kind {
        AgentKind::ClaudeCode => {
            let dir = || -> Result<PathBuf> {
                Ok(match &project_dir {
                    Some(d) => d.clone(),
                    None => std::env::current_dir()?,
                })
            };
            let scope = match scope {
                Scope::User => ClaudeScope::User,
                Scope::Project => ClaudeScope::Project(dir()?),
                Scope::Local => ClaudeScope::Local(dir()?),
            };
            install::claude_settings_path(&scope).context("no home directory for ~/.claude")
        }
        AgentKind::Codex => {
            if !matches!(scope, Scope::User) {
                bail!(
                    "Codex project hooks (.codex/hooks.json) load only in trusted projects; \
                     TailFlow installs Codex hooks per user. Use --config to target a file explicitly."
                );
            }
            install::codex_hooks_path().context("no home directory for ~/.codex")
        }
    }
}

/// The binary hooks should run: this one, by its canonical path.
fn hook_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine TailFlow's own path")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    Ok(exe)
}

fn looks_ephemeral(p: &Path) -> bool {
    let s = p.to_string_lossy();
    [
        "/_npx/",
        "\\_npx\\",
        "/target/debug/",
        "/tmp/",
        "/var/folders/",
    ]
    .iter()
    .any(|m| s.contains(m))
}

fn parse_version(s: &str) -> Result<(u32, u32, u32)> {
    tailflow_core::agent::codex::parse_version(s)
        .with_context(|| format!("invalid --codex-version {s:?} (expected X.Y.Z)"))
}

pub fn run(cmd: AgentCommand) -> Result<()> {
    match cmd {
        AgentCommand::Hook { agent } => run_hook(&agent),
        AgentCommand::List { json } => list(json),
        AgentCommand::Status { json } => status(json),
        AgentCommand::Install {
            agent,
            scope,
            project_dir,
            config,
            codex_version,
            dry_run,
        } => {
            let kind = parse_agent(&agent)?;
            let path = config_path(kind, scope, project_dir, config)?;
            let exe = hook_binary()?;
            let version = match (kind, codex_version) {
                (AgentKind::Codex, Some(v)) => Some(parse_version(&v)?),
                (AgentKind::Codex, None) => install::detect_codex_version(),
                _ => None,
            };
            if kind == AgentKind::Codex {
                match version {
                    Some(v) if v < tailflow_core::agent::codex::MIN_VERSION => bail!(
                        "Codex {}.{}.{} predates stable hooks (0.124.0); upgrade Codex first",
                        v.0, v.1, v.2
                    ),
                    Some(v) => println!("Codex {}.{}.{} detected", v.0, v.1, v.2),
                    None => println!(
                        "Codex version not detected (is `codex` on PATH?): installing only the \
                         events every hooks-capable release supports. Pass --codex-version to get more."
                    ),
                }
            }
            let events = install::events_for(kind, version);
            let plan = install::plan_install(kind, &path, &exe, &events)?;
            if dry_run {
                println!(
                    "{}: would add {:?}, update {:?}",
                    path.display(),
                    plan.added,
                    plan.updated
                );
                print!("{}", plan.render());
                return Ok(());
            }
            let backup = install::apply(&plan)?;
            let dir = agents_dir()?;
            let mut state = IntegrationState::load(&dir).unwrap_or_default();
            state.record(
                kind,
                IntegrationRecord {
                    config_path: path.clone(),
                    binary: exe.clone(),
                    events: events.iter().map(|e| (*e).to_string()).collect(),
                    installed_at: chrono::Utc::now(),
                    tailflow_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            );
            state.save(&dir)?;
            if plan.changes() {
                println!(
                    "installed TailFlow hooks for {} in {}",
                    kind.display_name(),
                    path.display()
                );
                if !plan.added.is_empty() {
                    println!("  added    {}", plan.added.join(", "));
                }
                if !plan.updated.is_empty() {
                    println!("  updated  {}", plan.updated.join(", "));
                }
                if let Some(b) = backup {
                    println!("  backup   {}", b.display());
                }
            } else {
                println!(
                    "{} hooks already installed in {} (no change)",
                    kind.display_name(),
                    path.display()
                );
            }
            println!("  binary   {}", exe.display());
            if looks_ephemeral(&exe) {
                println!(
                    "  warning  this binary path looks temporary (npx cache or a build dir); \
                     hooks break when it disappears. Install TailFlow globally and reinstall."
                );
            }
            warn_policy(kind);
            println!();
            match kind {
                AgentKind::ClaudeCode => println!(
                    "Next: start a new Claude Code session (hooks load at session start; \
                     interactive sessions run them only in trusted folders)."
                ),
                AgentKind::Codex => println!(
                    "Next: open Codex and run /hooks to review and trust the TailFlow hooks. \
                     Codex skips hooks you have not trusted, and asks again if they change."
                ),
            }
            println!(
                "Then: `tailflow sessions` (works offline), or run `tailflow-daemon --agents` to \
                 ingest continuously. TailFlow records metadata only; see `tailflow agent list`."
            );
            Ok(())
        }
        AgentCommand::Uninstall {
            agent,
            scope,
            project_dir,
            config,
            dry_run,
        } => {
            let kind = parse_agent(&agent)?;
            let path = config_path(kind, scope, project_dir, config)?;
            let plan = install::plan_uninstall(kind, &path)?;
            if dry_run {
                println!(
                    "{}: would remove {} TailFlow handler(s)",
                    path.display(),
                    plan.removed
                );
                return Ok(());
            }
            let backup = install::apply(&plan)?;
            if let Ok(dir) = agents_dir() {
                if let Ok(mut state) = IntegrationState::load(&dir) {
                    state.forget(kind, &path);
                    state.save(&dir)?;
                }
            }
            if plan.removed == 0 {
                println!(
                    "no TailFlow hooks for {} in {}",
                    kind.display_name(),
                    path.display()
                );
            } else {
                println!(
                    "removed {} TailFlow handler(s) from {}{}",
                    plan.removed,
                    path.display(),
                    backup
                        .map(|b| format!(" (backup: {})", b.display()))
                        .unwrap_or_default()
                );
            }
            println!(
                "Recorded history is kept. Delete it with `tailflow-daemon storage purge --session <id>` \
                 or `--all`."
            );
            Ok(())
        }
    }
}

fn warn_policy(kind: AgentKind) {
    match kind {
        AgentKind::ClaudeCode => {
            if let Some((p, setting)) = install::claude_policy_blocks_hooks() {
                println!(
                    "  warning  managed policy {} sets {setting}: Claude Code will not run user hooks",
                    p.display()
                );
            }
        }
        AgentKind::Codex => {
            for w in install::codex_config_warnings() {
                println!("  warning  {w}");
            }
        }
    }
}

fn level(l: CoverageLevel) -> &'static str {
    match l {
        CoverageLevel::Direct => "direct",
        CoverageLevel::Partial => "partial",
        CoverageLevel::None => "not captured",
    }
}

fn list(json: bool) -> Result<()> {
    if json {
        let v: Vec<serde_json::Value> = AgentKind::ALL
            .iter()
            .map(|k| serde_json::json!({"agent": k.as_str(), "name": k.display_name(), "capabilities": capabilities(*k)}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for k in AgentKind::ALL {
        println!("{} ({})", k.display_name(), k.as_str());
        for c in capabilities(k) {
            println!("  {:<18} {:<13} {}", c.activity, level(c.level), c.note);
        }
        println!();
    }
    println!("Never recorded by default: prompts, assistant messages, tool output, file contents,");
    println!("command arguments (only the program name). See docs/privacy.md.");
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let dir = agents_dir()?;
    let state = IntegrationState::load(&dir);
    let exe = hook_binary()?;
    let spool = Spool::at(dir.join("spool"));
    let stats = spool.stats();
    let policy = hook::load_policy(&dir);
    let mut agents = Vec::new();
    for kind in AgentKind::ALL {
        let recorded: Vec<IntegrationRecord> = state
            .as_ref()
            .ok()
            .and_then(|s| s.integrations.get(&kind).cloned())
            .unwrap_or_default();
        let default_path = match kind {
            AgentKind::ClaudeCode => install::claude_settings_path(&ClaudeScope::User),
            AgentKind::Codex => install::codex_hooks_path(),
        };
        let mut paths: Vec<PathBuf> = recorded.iter().map(|r| r.config_path.clone()).collect();
        if let Some(p) = default_path {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        let inspections: Vec<install::Inspection> = paths
            .iter()
            .map(|p| {
                let events: Vec<&str> = recorded
                    .iter()
                    .find(|r| &r.config_path == p)
                    .map(|r| r.events.iter().map(String::as_str).collect())
                    .unwrap_or_else(|| install::events_for(kind, None));
                install::inspect(kind, p, &exe, &events)
            })
            .collect();
        agents.push((kind, recorded, inspections));
    }
    if json {
        let v = serde_json::json!({
            "agents_dir": dir,
            "binary": exe,
            "config_error": policy.as_ref().err(),
            "spool": stats,
            "integrations": agents.iter().map(|(k, rec, ins)| serde_json::json!({
                "agent": k.as_str(), "recorded": rec, "configs": ins,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for (kind, recorded, inspections) in &agents {
        let installed: Vec<&install::Inspection> = inspections
            .iter()
            .filter(|i| !i.installed.is_empty())
            .collect();
        if installed.is_empty() {
            println!(
                "{:<12} not installed (`tailflow agent install {}`)",
                kind.display_name(),
                if *kind == AgentKind::ClaudeCode {
                    "claude"
                } else {
                    "codex"
                }
            );
            if !recorded.is_empty() {
                println!("             warning: TailFlow installed it before, but the hooks are gone from {}",
                    recorded.iter().map(|r| r.config_path.display().to_string()).collect::<Vec<_>>().join(", "));
            }
            continue;
        }
        for i in installed {
            println!(
                "{:<12} installed in {}",
                kind.display_name(),
                i.path.display()
            );
            println!("             events: {}", i.installed.join(", "));
            if !i.missing.is_empty() {
                println!(
                    "             missing: {} (re-run install)",
                    i.missing.join(", ")
                );
            }
            if !i.other_binary.is_empty() {
                println!("             note: hooks point at another TailFlow binary or settings; re-run install to update");
            }
            if !i.duplicated.is_empty() {
                println!(
                    "             warning: duplicated handlers for {}",
                    i.duplicated.join(", ")
                );
            }
            if i.hooks_disabled {
                println!("             warning: disableAllHooks is true in this file");
            }
        }
        if *kind == AgentKind::Codex {
            println!("             trust: Codex runs these only after you trust them in /hooks (TailFlow cannot check)");
        }
        let mut out = std::io::stdout();
        let _ = out.flush();
        warn_policy(*kind);
    }
    println!();
    match &policy {
        Ok(p) => println!(
            "capture      {} (per-user agent config: {})",
            p.mode.as_str(),
            hook::config_path(&dir).display()
        ),
        Err(e) => println!("capture      INVALID CONFIG — hooks record nothing until fixed: {e}"),
    }
    println!(
        "spool        {}{} waiting, {} claimed by a daemon{}",
        stats.pending_files,
        if stats.pending_capped { "+" } else { "" },
        stats.claimed_files,
        stats
            .oldest_pending_at
            .map(|t| format!(
                ", oldest {}",
                t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
            ))
            .unwrap_or_default()
    );
    if stats.overflow.count > 0 {
        println!(
            "             {} hook call(s) LOST to a full spool (recorded as a gap on ingest)",
            stats.overflow.count
        );
    }
    if stats.pending_files > 0 {
        println!("             not yet in history: run `tailflow sessions` or start `tailflow-daemon --agents`");
    }
    Ok(())
}

/// The hook bridge. Never writes to stdout, never fails the agent: every
/// path ends in exit 0. `TAILFLOW_HOOK_DEBUG=1` prints a report to stderr.
fn run_hook(agent: &str) -> Result<()> {
    let debug = std::env::var_os("TAILFLOW_HOOK_DEBUG").is_some_and(|v| !v.is_empty());
    if !debug {
        // A panic message on stderr would be shown to the user by the agent.
        std::panic::set_hook(Box::new(|_| {}));
    }
    let outcome = std::panic::catch_unwind(|| {
        let kind = AgentKind::parse(agent)?;
        let dir = tailflow_core::paths::default_agents_dir()?;
        let policy = hook::load_policy(&dir);
        let spool = Spool::at(dir.join("spool"));
        let mut stdin = std::io::stdin().lock();
        Some(hook::run(
            kind,
            &mut stdin,
            policy.as_ref().map_err(String::as_str),
            &spool,
        ))
    });
    if debug {
        eprintln!("tailflow hook: {outcome:?}");
    }
    std::process::exit(0);
}
