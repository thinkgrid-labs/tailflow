//! FTS maintenance under ingestion pressure (M2-H).
//!
//! Builds the M1.5-F worst case — a full-text index whose large segments are
//! mostly tombstones, so the first compaction is expensive — then ingests a
//! steady stream (default 4,000 ev/s, agent + runtime mix) into a small queue
//! while size retention evicts and compacts. Samples the writer every 10 ms
//! and reports the longest commit stall with work queued, the peak queue
//! depth, drops, and maintenance counters (when the build has them).
//!
//! Uses only API that exists in M1.5-F and M2, so the same file runs against
//! both: `cargo run --release -p tailflow-storage --example fts_yield -- --dir D`.

use chrono::Utc;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tailflow_core::config::{Retention, StorageConfig};
use tailflow_core::event::{EventStatus, ObservabilityEvent, Provenance};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};
use tailflow_core::LogLevel;
use tailflow_storage::{OpenOptions, Storage};

fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
    }
    None
}

fn event(i: u64) -> ObservabilityEvent {
    let agent = i.is_multiple_of(5);
    let mut e = if agent {
        let mut e = ObservabilityEvent::new(
            format!("agent:claude-code:bench:{i}"),
            ["agent.tool.completed", "command.started", "file.modified"][(i % 3) as usize],
            Provenance::AgentReported,
            "agent_hook",
            "claude-code",
            Utc::now(),
        );
        e.stable_id = true;
        e.session_id = Some(format!("claude-code:bench-{:03}", i % 40));
        e.agent_name = Some("claude-code".into());
        e.status = Some(EventStatus::Succeeded);
        e.message = Some(format!("Bash completed in {} ms", i % 900));
        e.path = Some(format!("/work/app/src/mod{}.rs", i % 70));
        e
    } else {
        let mut e = ObservabilityEvent::new(
            format!("rt:{i}"),
            "runtime.log",
            Provenance::RuntimeObserved,
            "process",
            "api",
            Utc::now(),
        );
        e.message = Some(format!(
            "GET /api/items/{} 200 {}ms user={} region=eu-west-{} cache=miss trace={:x}",
            i % 997,
            i % 250,
            i % 3001,
            i % 3,
            i.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ));
        e
    };
    e.severity = LogLevel::Info;
    e
}

fn open(path: &std::path::Path, cap_mb: u64, queue: usize) -> Storage {
    let cfg = StorageConfig {
        path: Some(path.to_path_buf()),
        max_size_mb: cap_mb,
        retention_days: Retention::Days(30),
        queue_capacity: queue,
        cleanup_interval_secs: 10,
        ..StorageConfig::default()
    };
    Storage::open(
        &cfg,
        OpenOptions {
            project_id: Some("/work/app".into()),
            version: "bench".into(),
            policy: Arc::new(CapturePolicy::from_config(&PrivacyConfig::default()).unwrap()),
        },
    )
    .unwrap()
}

fn main() {
    let dir = PathBuf::from(arg("--dir").expect("--dir DIR"));
    let prefill: u64 = arg("--prefill").map_or(1_200_000, |v| v.parse().unwrap());
    let delete_pct: u64 = arg("--delete-pct").map_or(75, |v| v.parse().unwrap());
    let rate: u64 = arg("--rate").map_or(4_000, |v| v.parse().unwrap());
    let seconds: u64 = arg("--seconds").map_or(60, |v| v.parse().unwrap());
    let queue: usize = arg("--queue").map_or(4_096, |v| v.parse().unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fts-yield.db");
    let _ = std::fs::remove_file(&path);

    // 1. Prefill with no cap pressure, then bloat the index: delete most rows
    //    (contentless FTS keeps tombstones in the big segments) without any
    //    merge, exactly the shape M1.5-F measured as the 5.4 s first heal.
    {
        let s = open(&path, 1_048_576, 1_000_000);
        for i in 0..prefill {
            let _ = s.submit(event(i));
        }
        s.flush(Duration::from_secs(900)).unwrap();
        s.shutdown(Duration::from_secs(60)).unwrap();
    }
    let used_before;
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.busy_timeout(Duration::from_secs(60)).unwrap();
        c.execute(
            "INSERT INTO events_fts(events_fts, rank) VALUES ('deletemerge', 0)",
            [],
        )
        .unwrap();
        let hi: i64 = c
            .query_row("SELECT max(seq) FROM events", [], |r| r.get(0))
            .unwrap();
        let cut = hi * i64::try_from(delete_pct).unwrap() / 100;
        let mut lo = 1i64;
        while lo < cut {
            let up = (lo + 20_000).min(cut);
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            c.execute(
                "DELETE FROM events_fts WHERE rowid BETWEEN ?1 AND ?2",
                [lo, up],
            )
            .unwrap();
            let n = c
                .execute("DELETE FROM events WHERE seq BETWEEN ?1 AND ?2", [lo, up])
                .unwrap();
            c.execute(
                "UPDATE meta SET value = CAST(value AS INTEGER) - ?1 WHERE key = 'event_count'",
                [i64::try_from(n).unwrap()],
            )
            .unwrap();
            c.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'evicted_through_seq'",
                [up],
            )
            .unwrap();
            c.execute_batch("COMMIT").unwrap();
            lo = up + 1;
        }
        let fts: i64 = c
            .query_row(
                "SELECT coalesce(sum(length(block)),0) FROM events_fts_data",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let rows: i64 = c
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
        let free: i64 = c
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        used_before = (pages - free) * 4096;
        eprintln!(
            "bloated: {rows} rows, fts {:.1} MB ({:.0} B/row), used {:.1} MB",
            fts as f64 / 1e6,
            fts as f64 / rows as f64,
            used_before as f64 / 1e6
        );
    }

    // 2. Cap below current use so retention evicts and owes a compaction,
    //    then ingest at `rate` while sampling the writer.
    let cap_mb = u64::try_from(used_before).unwrap() / (1024 * 1024) * 8 / 10;
    let s = open(&path, cap_mb.max(8), queue);
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let s = s.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut peak_depth = 0u64;
            let mut last_persisted = 0u64;
            let mut stall_start: Option<Instant> = None;
            let mut max_stall = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                let h = s.health();
                peak_depth = peak_depth.max(h.ingestion_queue_depth);
                if h.events_persisted > last_persisted || h.ingestion_queue_depth == 0 {
                    if let Some(t) = stall_start.take() {
                        max_stall = max_stall.max(t.elapsed());
                    }
                    last_persisted = h.events_persisted;
                } else if stall_start.is_none() {
                    stall_start = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            (peak_depth, max_stall)
        })
    };
    let started = Instant::now();
    let mut i = prefill;
    let mut sent = 0u64;
    while started.elapsed() < Duration::from_secs(seconds) {
        let due = (started.elapsed().as_secs_f64() * rate as f64) as u64;
        while sent < due {
            let _ = s.submit(event(i));
            i += 1;
            sent += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    s.flush(Duration::from_secs(120)).unwrap();
    stop.store(true, Ordering::Relaxed);
    let (peak_depth, max_stall) = sampler.join().unwrap();
    let h = serde_json::to_value(s.health()).unwrap();
    let st = serde_json::to_value(s.status().unwrap()).unwrap();
    s.shutdown(Duration::from_secs(60)).unwrap();
    let c = rusqlite::Connection::open(&path).unwrap();
    let fts: i64 = c
        .query_row(
            "SELECT coalesce(sum(length(block)),0) FROM events_fts_data",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let out = serde_json::json!({
        "rate": rate, "seconds": seconds, "queue_capacity": queue, "cap_mb": cap_mb,
        "submitted": sent,
        "persisted": h["events_persisted"],
        "dropped_queue_full": h["events_dropped"]["queue_full"],
        "peak_queue_depth": peak_depth,
        "max_commit_stall_ms": max_stall.as_millis(),
        "maintenance": h.get("maintenance"),
        "last_retention": st["retention"]["last_cleanup"],
        "fts_bytes_per_row_after": fts as f64 / rows.max(1) as f64,
    });
    println!("{out}");
}
