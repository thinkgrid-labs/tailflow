//! `tailflow-daemon storage …` — offline inspection and maintenance.
//!
//! Destructive actions live here, not behind HTTP: deleting history requires
//! a local shell with write access to the database file, which is a stronger
//! boundary than anything a loopback endpoint could check.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use tailflow_core::config::StorageConfig;
use tailflow_storage::admin::{self, PurgeTarget};

#[derive(Subcommand)]
pub enum StorageAction {
    /// Print the database path
    Path,
    /// Show size, event count, retained horizon, and recorded gaps
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Run a full integrity check (read-only)
    Check,
    /// Delete history (all, older than a point in time, or one session)
    Purge {
        /// Delete every event
        #[arg(long, conflicts_with_all = ["before", "session"])]
        all: bool,
        /// Delete events received before this point: RFC 3339 or 7d / 12h ago
        #[arg(long, value_name = "WHEN", conflicts_with = "session")]
        before: Option<String>,
        /// Delete one agent session's events
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Actually delete. Without it, only report what would be deleted.
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(action: StorageAction, cfg: &StorageConfig) -> Result<()> {
    let path = tailflow_storage::resolve_path(cfg)?;
    match action {
        StorageAction::Path => {
            println!("{}", path.display());
            Ok(())
        }
        StorageAction::Status { json } => {
            if !path.exists() {
                bail!("no database at {} (nothing recorded yet)", path.display());
            }
            let stats = admin::status(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("database   {}", path.display());
                println!(
                    "size       {:.1} MB file + {:.1} MB WAL ({:.1} MB live)",
                    mb(stats.file_bytes),
                    mb(stats.wal_bytes),
                    mb(stats.used_bytes)
                );
                println!("events     {}", stats.events);
                println!(
                    "oldest     {}",
                    stats
                        .oldest_event_at
                        .map_or_else(|| "-".into(), |t| t.to_rfc3339())
                );
                println!(
                    "newest     {}",
                    stats
                        .newest_event_at
                        .map_or_else(|| "-".into(), |t| t.to_rfc3339())
                );
                println!("expired    through seq {}", stats.evicted_through_seq);
                println!("gaps       {} recorded", stats.gaps);
                if let Some(g) = stats.last_gap {
                    println!(
                        "last gap   {} ({}) after seq {}",
                        g.reason,
                        g.count
                            .map_or_else(|| "unknown count".into(), |n| format!("{n} events")),
                        g.from_seq
                    );
                }
            }
            Ok(())
        }
        StorageAction::Check => {
            let problems = admin::check(&path)?;
            if problems.is_empty() {
                println!("ok: {} passed integrity_check", path.display());
                Ok(())
            } else {
                for p in &problems {
                    eprintln!("{p}");
                }
                bail!(
                    "{} integrity problem(s); the file was not modified",
                    problems.len()
                )
            }
        }
        StorageAction::Purge {
            all,
            before,
            session,
            yes,
        } => {
            let target = if all {
                PurgeTarget::All
            } else if let Some(b) = &before {
                PurgeTarget::Before(
                    tailflow_core::query::parse_since(b)
                        .with_context(|| format!("invalid --before {b:?}"))?,
                )
            } else if let Some(s) = &session {
                PurgeTarget::Session(s)
            } else {
                bail!("choose what to delete: --all, --before <WHEN>, or --session <ID>");
            };
            if !path.exists() {
                bail!("no database at {}", path.display());
            }
            if !yes {
                let stats = admin::status(&path)?;
                eprintln!(
                    "would delete {} from {} ({} events stored). Re-run with --yes to delete.",
                    describe(&target),
                    path.display(),
                    stats.events
                );
                std::process::exit(2);
            }
            let report = admin::purge(&path, target)?;
            println!(
                "deleted {} events; returned {} free pages to the filesystem",
                report.deleted, report.vacuumed_pages
            );
            Ok(())
        }
    }
}

fn describe(t: &PurgeTarget<'_>) -> String {
    match t {
        PurgeTarget::All => "ALL history".into(),
        PurgeTarget::Before(t) => format!("events received before {}", t.to_rfc3339()),
        PurgeTarget::Session(s) => format!("session {s:?}"),
    }
}

#[allow(clippy::cast_precision_loss)]
fn mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}
