//! Storage benchmarks for the M1 performance report.
//!
//! ```text
//! cargo run --release -p tailflow-storage --example bench_storage -- [--events N] [--dir PATH]
//! ```
//!
//! Prints one JSON object per measurement on stdout. Uses only the public
//! `Storage` API, i.e. the same path the daemon uses.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tailflow_core::config::{Retention, StorageConfig};
use tailflow_core::event::{EventStatus, ObservabilityEvent, Provenance};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig, Redactor};
use tailflow_core::{LogLevel, LogRecord};
use tailflow_storage::{EventQuery, OpenOptions, Order, Storage};

/// Deterministic xorshift so runs are reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const ROUTES: &[&str] = &[
    "/api/users",
    "/api/orders/42",
    "/health",
    "/api/auth/login",
    "/static/app.js",
];
const SOURCES: &[&str] = &["api", "web", "worker", "postgres", "redis", "frontend"];

/// Realistic runtime-log mix: ~55% access lines (~90 B), ~25% app lines
/// (~180 B), ~12% JSON (~350 B), ~5% warnings, ~2% errors, ~1% stack frames.
fn runtime_line(r: &mut Rng, i: u64) -> (String, LogLevel, &'static str) {
    let source = SOURCES[r.below(SOURCES.len() as u64) as usize];
    let roll = r.below(100);
    let (payload, level) = match roll {
        0..=54 => (
            format!(
                "{} GET {} {} {}ms ua=Mozilla/5.0 req={:08x}",
                Utc::now().format("%H:%M:%S%.3f"),
                ROUTES[r.below(ROUTES.len() as u64) as usize],
                [200, 200, 200, 304, 404][r.below(5) as usize],
                r.below(400),
                r.next() as u32
            ),
            LogLevel::Info,
        ),
        55..=79 => (
            format!(
                "INFO [{source}] processed job id={} in {}ms queue=default attempts=1 batch={} tenant=acme-{}",
                i,
                r.below(900),
                r.below(64),
                r.below(50)
            ),
            LogLevel::Info,
        ),
        80..=91 => (
            format!(
                r#"{{"level":"info","ts":"{}","msg":"request completed","method":"POST","path":"/api/orders","status":201,"duration_ms":{},"user_id":"u_{}","trace_id":"{:016x}","span_id":"{:08x}","bytes":{}}}"#,
                Utc::now().to_rfc3339(),
                r.below(300),
                r.below(10_000),
                r.next(),
                r.next() as u32,
                r.below(100_000)
            ),
            LogLevel::Info,
        ),
        92..=96 => (
            format!("WARN slow query took {}ms: SELECT * FROM orders WHERE customer_id = $1", 500 + r.below(4000)),
            LogLevel::Warn,
        ),
        97..=98 => (
            format!(
                "ERROR connection refused: postgres:{} (attempt {}) password=hunter{}",
                5432 + r.below(2),
                r.below(5),
                r.below(100)
            ),
            LogLevel::Error,
        ),
        _ => (
            format!("    at Pool.connect (node_modules/pg-pool/index.js:{}:{})", r.below(400), r.below(40)),
            LogLevel::Unknown,
        ),
    };
    (payload, level, source)
}

fn runtime_event(r: &mut Rng, i: u64) -> ObservabilityEvent {
    let (payload, level, source) = runtime_line(r, i);
    let rec = LogRecord {
        timestamp: Utc::now(),
        source: source.into(),
        level,
        payload,
    };
    ObservabilityEvent::from_log_record(&rec, format!("bench:{i}"), "process", Some(i))
}

/// Representative agent metadata (schema exercised now; collectors are M2).
fn agent_event(r: &mut Rng, i: u64) -> ObservabilityEvent {
    let session = format!("sess-{:04}", r.below(500));
    let (ty, path, cmd) = match r.below(6) {
        0 => ("agent.tool.started", None, None),
        1 => ("agent.tool.completed", None, None),
        2 => (
            "file.modified",
            Some(format!(
                "/work/app/src/{}/mod{}.rs",
                ["auth", "api", "db", "ui"][r.below(4) as usize],
                r.below(40)
            )),
            None,
        ),
        3 => (
            "file.read",
            Some(format!("/work/app/src/lib{}.rs", r.below(80))),
            None,
        ),
        4 => (
            "command.completed",
            None,
            Some(format!("cargo test -p crate{}", r.below(8))),
        ),
        _ => ("command.failed", None, Some("npm run build".to_string())),
    };
    let mut e = ObservabilityEvent::new(
        format!("agent:{i}"),
        ty,
        Provenance::AgentReported,
        "hook",
        "claude",
        Utc::now(),
    );
    e.session_id = Some(session);
    e.agent_name = Some(["claude", "codex", "cursor"][r.below(3) as usize].into());
    e.operation_id = Some(format!("op-{i}"));
    e.path = path;
    e.duration_ms = Some(r.below(5000));
    e.status = Some(if ty.ends_with("failed") {
        EventStatus::Failed
    } else {
        EventStatus::Succeeded
    });
    if let Some(c) = cmd {
        e.attributes.insert("command".into(), c.into());
        e.attributes.insert(
            "exit_code".into(),
            (if ty.ends_with("failed") { 1 } else { 0 }).into(),
        );
    }
    e.message = Some(format!(
        "{ty} by {}",
        e.agent_name.as_deref().unwrap_or("?")
    ));
    e
}

fn mixed(r: &mut Rng, i: u64) -> ObservabilityEvent {
    if r.below(5) == 0 {
        agent_event(r, i)
    } else {
        runtime_event(r, i)
    }
}

fn cfg(path: &Path) -> StorageConfig {
    StorageConfig {
        path: Some(path.to_path_buf()),
        max_size_mb: 1_048_576,
        retention_days: Retention::Days(30),
        queue_capacity: 1_000_000,
        ..StorageConfig::default()
    }
}

fn open(c: &StorageConfig) -> Storage {
    Storage::open(
        c,
        OpenOptions {
            project_id: Some("/work/app".into()),
            version: "bench".into(),
            policy: Arc::new(CapturePolicy::from_config(&PrivacyConfig::default()).unwrap()),
        },
    )
    .unwrap()
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summary(mut v: Vec<f64>) -> serde_json::Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({"n": v.len(), "p50": pct(&v, 0.50), "p95": pct(&v, 0.95), "p99": pct(&v, 0.99), "max": v.last()})
}

fn file_bytes(path: &Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(path).map_or(0, |m| m.len())
        + std::fs::metadata(PathBuf::from(wal)).map_or(0, |m| m.len())
}

fn emit(name: &str, v: serde_json::Value) {
    println!("{}", json!({"bench": name, "result": v}));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let big_n: u64 = arg("--events").map_or(3_000_000, |v| v.parse().unwrap());
    let root = arg("--dir").map_or_else(
        || std::env::temp_dir().join("tailflow-bench"),
        PathBuf::from,
    );
    if arg("--db").is_none() {
        let _ = std::fs::remove_dir_all(&root);
    }
    std::fs::create_dir_all(&root).unwrap();
    let only = arg("--only");
    let want = |n: &str| only.as_deref().is_none_or(|o| o.split(',').any(|x| x == n));

    // 1. Redaction cost per line (the hot path on every live record).
    if want("redaction") {
        let redactor = Redactor::builtin();
        let mut r = Rng(7);
        let lines: Vec<String> = (0..200_000).map(|i| runtime_line(&mut r, i).0).collect();
        let t = Instant::now();
        let mut hits = 0;
        for l in &lines {
            if redactor.redact(l).is_some() {
                hits += 1;
            }
        }
        let el = t.elapsed();
        emit(
            "redaction",
            json!({"lines": lines.len(), "redacted": hits, "ns_per_line": el.as_nanos() as f64 / lines.len() as f64}),
        );
    }

    // 2. Batching effect and write throughput.
    if want("batching") {
        for (batch, fts) in [(1usize, true), (64, true), (512, true), (512, false)] {
            let path = root.join(format!("batch-{batch}-{fts}.db"));
            let mut c = cfg(&path);
            c.batch_max_events = batch;
            c.full_text_search = fts;
            let s = open(&c);
            let n: u64 = if batch == 1 { 20_000 } else { 200_000 };
            let mut r = Rng(11);
            let events: Vec<_> = (0..n).map(|i| mixed(&mut r, i)).collect();
            let t = Instant::now();
            for e in events {
                let _ = s.submit(e);
            }
            s.flush(Duration::from_secs(600)).unwrap();
            let el = t.elapsed().as_secs_f64();
            let h = s.health();
            s.shutdown(Duration::from_secs(60)).unwrap();
            emit(
                "write_throughput",
                json!({
                    "batch_max_events": batch, "fts": fts, "events": n,
                    "events_per_sec": n as f64 / el, "seconds": el,
                    "commits": h.commits, "dropped": h.events_dropped_total,
                    "bytes_per_event": file_bytes(&path) as f64 / n as f64,
                }),
            );
        }
    }

    // 3. Persistence latency at fixed rates, measured as flush round-trip
    //    probes (time until everything submitted so far is committed).
    if want("latency") {
        for rate in [10u64, 100, 1_000, 10_000] {
            let path = root.join(format!("rate-{rate}.db"));
            let s = Arc::new(open(&cfg(&path)));
            let secs = if rate >= 1000 { 10 } else { 20 };
            let total = rate * secs;
            let probes = {
                let s = s.clone();
                std::thread::spawn(move || {
                    let mut samples = Vec::new();
                    let end = Instant::now() + Duration::from_secs(secs);
                    while Instant::now() < end {
                        std::thread::sleep(Duration::from_millis(50));
                        let t = Instant::now();
                        s.flush(Duration::from_secs(30)).unwrap();
                        samples.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    samples
                })
            };
            let mut r = Rng(13);
            let start = Instant::now();
            let interval = Duration::from_secs_f64(1.0 / rate as f64);
            for i in 0..total {
                let due = start + interval * u32::try_from(i).unwrap();
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                let _ = s.submit(mixed(&mut r, i));
            }
            let samples = probes.join().unwrap();
            s.flush(Duration::from_secs(60)).unwrap();
            let h = s.health();
            emit(
                "persist_latency_ms",
                json!({
                    "rate": rate, "events": total, "achieved_rate": total as f64 / start.elapsed().as_secs_f64(),
                    "persisted": h.events_persisted, "dropped": h.events_dropped_total,
                    "flush_roundtrip_ms": summary(samples), "commits": h.commits,
                }),
            );
            s.shutdown(Duration::from_secs(60)).unwrap();
        }
    }

    // Size eviction alone on an existing database copy (`--db PATH`), with a
    // per-phase breakdown of every pass.
    if want("size_existing") {
        let db = PathBuf::from(arg("--db").expect("--db PATH"));
        let mut c = cfg(&db);
        c.cleanup_interval_secs = 86_400;
        let probe = open(&c);
        let used = probe.status().unwrap().db.used_bytes;
        probe.shutdown(Duration::from_secs(60)).unwrap();
        drop(probe);
        c.max_size_mb = (used / 2 / (1024 * 1024)).max(16);
        let s = open(&c);
        let t = Instant::now();
        let mut worst = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        let (mut passes, mut evicted) = (0, 0);
        loop {
            let pt = Instant::now();
            let r = s.run_retention(Duration::from_secs(600)).unwrap();
            let total = pt.elapsed().as_secs_f64() * 1000.0;
            passes += 1;
            evicted += r.evicted_by_size;
            worst.0 = worst.0.max(total);
            worst.1 = worst.1.max(r.age_ms);
            worst.2 = worst.2.max(r.payload_ms);
            worst.3 = worst.3.max(r.size_ms);
            worst.4 = worst.4.max(r.vacuum_ms);
            if !r.more_work {
                break;
            }
        }
        let after = s.status().unwrap().db;
        emit(
            "retention_size_isolated",
            json!({
                "cap_mb": c.max_size_mb, "used_before": used, "used_after": after.used_bytes,
                "evicted": evicted, "passes": passes, "total_ms": t.elapsed().as_secs_f64() * 1000.0,
                "max_pass_ms": worst.0, "max_age_ms": worst.1, "max_payload_ms": worst.2,
                "max_size_ms": worst.3, "max_vacuum_checkpoint_ms": worst.4,
            }),
        );
        s.shutdown(Duration::from_secs(60)).unwrap();
        return;
    }

    // 4. Large database: build, measure growth, queries, retention, restart.
    let big = root.join("big.db");
    if want("big") {
        let c = cfg(&big);
        let s = open(&c);
        let mut r = Rng(17);
        let start_at = Utc::now() - ChronoDuration::days(33);
        let step = ChronoDuration::days(33).num_microseconds().unwrap() / big_n as i64;
        let t = Instant::now();
        for i in 0..big_n {
            let mut e = mixed(&mut r, i);
            let when = start_at + ChronoDuration::microseconds(step * i as i64);
            e.timestamp = when;
            e.observed_at = when;
            let _ = s.submit(e);
            if i % 200_000 == 0 {
                // Keep the queue bounded while loading.
                s.flush(Duration::from_secs(600)).unwrap();
            }
        }
        s.flush(Duration::from_secs(600)).unwrap();
        let load = t.elapsed().as_secs_f64();
        // The startup retention pass ran before the load; nothing expired yet.
        let st = s.status().unwrap();
        emit(
            "bulk_load",
            json!({
                "events": big_n, "seconds": load, "events_per_sec": big_n as f64 / load,
                "file_bytes": st.db.file_bytes, "wal_bytes": st.db.wal_bytes,
                "bytes_per_event": (st.db.file_bytes + st.db.wal_bytes) as f64 / big_n as f64,
                "dropped": s.health().events_dropped_total,
            }),
        );

        // Queries.
        let newest = st.db.newest_seq.unwrap();
        let since7 = Utc::now() - ChronoDuration::days(7);
        let since1 = Utc::now() - ChronoDuration::days(1);
        let qs: Vec<(&str, EventQuery)> = vec![
            (
                "latest_100_desc",
                EventQuery {
                    order: Order::Desc,
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "runtime_error_since_7d",
                EventQuery {
                    event_types: vec!["runtime.error".into()],
                    since: Some(since7),
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "runtime_error_since_7d_desc",
                EventQuery {
                    event_types: vec!["runtime.error".into()],
                    since: Some(since7),
                    order: Order::Desc,
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "session_lookup",
                EventQuery {
                    sessions: vec!["sess-0042".into()],
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "agent_file_modified_24h",
                EventQuery {
                    agents: vec!["claude".into()],
                    event_types: vec!["file.modified".into()],
                    since: Some(since1),
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "source_warn_24h",
                EventQuery {
                    sources: vec!["api".into()],
                    min_severity: Some(LogLevel::Warn),
                    since: Some(since1),
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "path_glob_auth_7d",
                EventQuery {
                    path_glob: Some("**/auth/**".into()),
                    event_types: vec!["file.modified".into()],
                    since: Some(since7),
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "command_failed_project",
                EventQuery {
                    event_types: vec!["command.failed".into()],
                    projects: vec!["/work/app".into()],
                    order: Order::Desc,
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "fts_common_desc",
                EventQuery {
                    text: Some("connection refused".into()),
                    order: Order::Desc,
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "fts_rare_term",
                EventQuery {
                    text: Some("hunter42".into()),
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "unindexed_severity_scan_page",
                EventQuery {
                    min_severity: Some(LogLevel::Error),
                    since: Some(since1),
                    limit: 100,
                    ..Default::default()
                },
            ),
            (
                "cursor_tail_after_newest",
                EventQuery {
                    after_seq: Some(newest - 50),
                    limit: 100,
                    ..Default::default()
                },
            ),
        ];
        for (name, q) in qs {
            let mut samples = Vec::new();
            let mut rows = 0;
            let mut budget_hit = false;
            for _ in 0..20 {
                let t = Instant::now();
                let p = s.query(&q).unwrap();
                samples.push(t.elapsed().as_secs_f64() * 1000.0);
                rows = p.events.len();
                budget_hit = p.scan_budget_exhausted;
            }
            emit(
                "query_ms",
                json!({"query": name, "rows": rows, "scan_budget_exhausted": budget_hit, "latency": summary(samples)}),
            );
        }
        let mut samples = Vec::new();
        for i in 0..200 {
            let t = Instant::now();
            let _ = s.get(1 + (i * 7919) % newest, false).unwrap();
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(
            "query_ms",
            json!({"query": "get_by_seq", "latency": summary(samples)}),
        );
        let mut samples = Vec::new();
        for _ in 0..50 {
            let t = Instant::now();
            let _ = s.status().unwrap();
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(
            "query_ms",
            json!({"query": "status_endpoint", "latency": summary(samples)}),
        );

        // Retention: ~3 of 33 days are past the 30-day horizon. Meanwhile a
        // producer submits at 1,000 events/s and a prober measures how long
        // persistence stalls behind cleanup (retention runs on the writer).
        let s = Arc::new(s);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer = {
            let (s, stop) = (s.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut r = Rng(19);
                let mut i = big_n;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = s.submit(mixed(&mut r, i));
                    i += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let prober = {
            let (s, stop) = (s.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut v = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    let t = Instant::now();
                    s.flush(Duration::from_secs(60)).unwrap();
                    v.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                v
            })
        };
        let t = Instant::now();
        let mut passes = Vec::new();
        let mut expired = 0;
        loop {
            let pt = Instant::now();
            let rep = s.run_retention(Duration::from_secs(120)).unwrap();
            passes.push(pt.elapsed().as_secs_f64() * 1000.0);
            expired += rep.expired_by_age;
            if !rep.more_work {
                break;
            }
        }
        let total_ms = t.elapsed().as_secs_f64() * 1000.0;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        producer.join().unwrap();
        let stalls = prober.join().unwrap();
        emit(
            "retention_age",
            json!({
                "expired": expired, "total_ms": total_ms,
                "passes": passes.len(), "max_pass_ms": passes.iter().cloned().fold(0.0, f64::max),
                "pass_budget_ms": 250,
                "concurrent_ingest_flush_roundtrip_ms": summary(stalls),
                "dropped_during_cleanup": s.health().events_dropped_total,
            }),
        );

        s.shutdown(Duration::from_secs(120)).unwrap();
        drop(s);

        let t = Instant::now();
        let s = open(&c);
        let clean_open = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let _ = s
            .query(&EventQuery {
                order: Order::Desc,
                limit: 100,
                ..Default::default()
            })
            .unwrap();
        let first_query = t.elapsed().as_secs_f64() * 1000.0;
        s.shutdown(Duration::from_secs(60)).unwrap();
        drop(s);
        // Unclean: forge a dead run so open() performs recovery + quick_check.
        {
            let conn = rusqlite_conn(&big);
            conn.execute("INSERT INTO runs (run_key, started_at, heartbeat_at, version, pid, last_seq) VALUES ('dead', 1, 1, 'bench', 2147483000, 1)", []).unwrap();
        }
        let t = Instant::now();
        let s = open(&c);
        let unclean_open = t.elapsed().as_secs_f64() * 1000.0;
        emit(
            "restart_ms",
            json!({
                "events": s.status().unwrap().db.events, "clean_open_ms": clean_open,
                "first_query_ms": first_query, "unclean_open_with_quick_check_ms": unclean_open,
            }),
        );

        // Size-cap eviction: lower the cap to half the live size.
        let used = s.status().unwrap().db.used_bytes;
        s.shutdown(Duration::from_secs(60)).unwrap();
        drop(s);
        let mut c2 = c.clone();
        c2.max_size_mb = (used / 2 / (1024 * 1024)).max(16);
        c2.cleanup_interval_secs = 86_400;
        let s = open(&c2);
        let t = Instant::now();
        let mut evicted = 0;
        let mut passes = Vec::new();
        loop {
            let pt = Instant::now();
            let rep = s.run_retention(Duration::from_secs(120)).unwrap();
            passes.push(pt.elapsed().as_secs_f64() * 1000.0);
            evicted += rep.evicted_by_size;
            if !rep.more_work {
                break;
            }
        }
        let after = s.status().unwrap().db;
        emit(
            "retention_size",
            json!({
                "cap_mb": c2.max_size_mb, "used_before": used, "used_after": after.used_bytes,
                "file_after": after.file_bytes, "evicted": evicted,
                "total_ms": t.elapsed().as_secs_f64() * 1000.0, "passes": passes.len(),
                "max_pass_ms": passes.iter().cloned().fold(0.0, f64::max),
            }),
        );
        s.shutdown(Duration::from_secs(60)).unwrap();
    }
}

fn rusqlite_conn(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}
