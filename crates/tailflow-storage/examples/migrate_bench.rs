//! First-open cost of the schema v3 → v4 migration on a large database.
//!
//!   cargo run --release -p tailflow-storage --example migrate_bench -- <events> [dir]
//!
//! Builds a v3 database of `<events>` rows (85 % runtime log lines without
//! a path, 15 % agent hook events with a path, the mix of an active agent
//! user), then times `schema::open_writer`, which applies v4 (one table,
//! two partial indexes). Prints one JSON line.

use rusqlite::{params, Connection};
use std::time::Instant;
use tailflow_core::config::SyncMode;
use tailflow_storage::schema;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let n: i64 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let dir = args
        .get(2)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("tf-migrate-{n}"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("events.db");
    let _fresh = std::fs::remove_file(&path);
    for ext in ["-wal", "-shm"] {
        let _fresh = std::fs::remove_file(dir.join(format!("events.db{ext}")));
    }

    // A v3 database, built with the real v1–v3 migration SQL.
    let built = Instant::now();
    {
        let mut c = Connection::open(&path)?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "application_id", schema::APPLICATION_ID)?;
        for m in schema::MIGRATIONS.iter().filter(|m| m.version <= 3) {
            c.execute_batch(m.sql)?;
        }
        c.pragma_update(None, "user_version", 3)?;
        let tx = c.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO events (event_id, schema_version, ts, observed_at, event_type, provenance,
                 source_type, source_id, project_id, session_id, agent_name, severity, path, message, run_id)
                 VALUES (?1, 1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 2, ?10, ?11, 1)",
            )?;
            for i in 0..n {
                let ts = 1_700_000_000_000_000i64 + i * 1_000;
                if i % 100 < 85 {
                    ins.execute(params![
                        format!("r:{i}"),
                        ts,
                        "runtime.log",
                        "runtime_observed",
                        "process",
                        "api",
                        Some("/work/app"),
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        format!("GET /api/items/{i} 200 3ms"),
                    ])?;
                } else {
                    let ty = if i % 3 == 0 {
                        "file.modified"
                    } else {
                        "agent.tool.completed"
                    };
                    ins.execute(params![
                        format!("a:{i}"),
                        ts,
                        ty,
                        "agent_reported",
                        "agent_hook",
                        "claude-code",
                        Some("/work/app"),
                        Some(format!("claude-code:s{}", i / 5_000)),
                        Some("claude-code"),
                        Some(format!("/work/app/src/mod{}.rs", i % 2_000)),
                        "Edit completed",
                    ])?;
                }
            }
        }
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'event_count'",
            [n.to_string()],
        )?;
        tx.commit()?;
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    }
    let build_s = built.elapsed().as_secs_f64();
    let bytes = std::fs::metadata(&path)?.len();

    let t = Instant::now();
    let c = schema::open_writer(&path, SyncMode::Normal)?;
    let migrate_s = t.elapsed().as_secs_f64();
    let v: i32 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let indexed: i64 = c.query_row(
        "SELECT count(*) FROM events INDEXED BY events_path_ts
         WHERE path IS NOT NULL AND source_type IN ('fs_watcher','agent_hook')",
        [],
        |r| r.get(0),
    )?;
    drop(c);
    let after = std::fs::metadata(&path)?.len();
    println!(
        "{}",
        serde_json::json!({
            "events": n,
            "db_bytes_before": bytes,
            "db_bytes_after": after,
            "build_s": build_s,
            "migrate_v3_to_v4_s": migrate_s,
            "schema": v,
            "rows_in_events_path_ts": indexed,
        })
    );
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
