//! Latency of the dashboard aggregates (M3) on a large database.
//!
//! ```text
//! cargo run --release -p tailflow-storage --example bench_stats -- \
//!     [--events 2000000] [--agent-share 0.03] [--dir PATH] [--reuse]
//! ```
//!
//! Builds (or reuses) a database whose events span the last 7 days in time
//! order: runtime logs (≈ 5 % errors, 8 % warnings) and agent sessions
//! (Claude Code and Codex: tool calls with start/end, commands, failures,
//! file events). Then times `stats` (1 h / 24 h / 7 d), `files` (7 d,
//! multi-agent), session pages (plain and status-filtered) and an
//! `agent.*` event page through the public `Storage` API — the daemon's path.
//! Prints one JSON object per measurement.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tailflow_core::config::StorageConfig;
use tailflow_core::event::{EventStatus, ObservabilityEvent, Provenance};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};
use tailflow_core::LogLevel;
use tailflow_storage::sessions::SessionQuery;
use tailflow_storage::stats::{FilesQuery, StatsQuery};
use tailflow_storage::{EventQuery, OpenOptions, Order, Storage};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn open(path: &std::path::Path) -> Storage {
    let cfg = StorageConfig {
        path: Some(path.to_path_buf()),
        max_size_mb: 16_384,
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

fn agent_ev(
    id: String,
    ty: &str,
    agent: &str,
    session: &str,
    ts: chrono::DateTime<Utc>,
) -> ObservabilityEvent {
    let mut e = ObservabilityEvent::new(
        id,
        ty,
        Provenance::AgentReported,
        tailflow_core::agent::SOURCE_TYPE,
        agent,
        ts,
    );
    e.observed_at = ts;
    e.stable_id = true;
    e.agent_name = Some(agent.into());
    e.session_id = Some(format!("{agent}:{session}"));
    e.project_id = Some(
        if session.ends_with('0') {
            "/work/web"
        } else {
            "/work/app"
        }
        .into(),
    );
    e
}

/// Backpressured submit: a dropped event would record a loss marker stamped
/// "now", which raises the `observed_at` floor and collapses the synthetic
/// history onto the present. Nothing may be dropped here.
fn put(s: &Storage, e: ObservabilityEvent) {
    let r = s.submit_backpressured(e, Instant::now() + Duration::from_secs(600));
    assert!(
        matches!(r, tailflow_storage::Submitted::Enqueued),
        "event not enqueued: {r:?}"
    );
}

fn build(path: &std::path::Path, n: u64, agent_share: f64) {
    let s = open(path);
    let mut r = Rng(42);
    let start = Utc::now() - ChronoDuration::days(7);
    let step_us = (7 * 86_400 * 1_000_000) / n.max(1);
    let agent_every = (1.0 / agent_share).round().max(1.0) as u64;
    let mut op = 0u64;
    for i in 0..n {
        let ts = start + ChronoDuration::microseconds((i * step_us) as i64);
        if i.is_multiple_of(agent_every) {
            // One agent operation: tool start/end, sometimes a command, file event.
            op += 1;
            let agent = if op.is_multiple_of(3) {
                "codex"
            } else {
                "claude-code"
            };
            let session = format!("s{}", op / 400); // ~400 operations per session
            let tool = format!("tool:{op}");
            let mut start_ev =
                agent_ev(format!("a{op}s"), "agent.tool.started", agent, &session, ts);
            start_ev.operation_id = Some(tool.clone());
            start_ev.status = Some(EventStatus::Started);
            let failed = r.below(20) == 0;
            let mut end = agent_ev(
                format!("a{op}e"),
                if failed {
                    "agent.tool.failed"
                } else {
                    "agent.tool.completed"
                },
                agent,
                &session,
                ts + ChronoDuration::milliseconds(800),
            );
            end.operation_id = Some(tool.clone());
            end.duration_ms = Some(800);
            end.status = Some(if failed {
                EventStatus::Failed
            } else {
                EventStatus::Succeeded
            });
            put(&s, start_ev);
            if op.is_multiple_of(4) {
                let mut c = agent_ev(
                    format!("a{op}c"),
                    if failed {
                        "command.failed"
                    } else {
                        "command.started"
                    },
                    agent,
                    &session,
                    ts,
                );
                c.parent_operation_id = Some(tool);
                c.operation_id = Some(format!("cmd:{op}"));
                c.attributes.insert(
                    "command.program".into(),
                    json!(["cargo", "npm", "make"][(op % 3) as usize]),
                );
                put(&s, c);
            } else if op % 4 == 1 {
                let mut f = agent_ev(
                    format!("a{op}f"),
                    if op % 8 == 1 {
                        "file.read"
                    } else {
                        "file.modified"
                    },
                    agent,
                    &session,
                    ts,
                );
                f.path = Some(format!("/work/app/src/m{}.rs", r.below(500)));
                put(&s, f);
            }
            put(&s, end);
        } else {
            let lvl = match r.below(100) {
                0..=4 => LogLevel::Error,
                5..=12 => LogLevel::Warn,
                _ => LogLevel::Info,
            };
            let rec = tailflow_core::LogRecord {
                timestamp: ts,
                source: ["api", "web", "worker"][(i % 3) as usize].into(),
                level: lvl,
                payload: format!(
                    "GET /api/items/{} 200 {}ms request_id={:x}",
                    r.below(10_000),
                    r.below(400),
                    r.next()
                ),
            };
            let mut e =
                ObservabilityEvent::from_log_record(&rec, format!("run:{i}"), "process", Some(i));
            e.observed_at = ts;
            put(&s, e);
        }
        if i.is_multiple_of(200_000) && i > 0 {
            s.flush(Duration::from_secs(600)).unwrap();
            eprintln!("{i} events");
        }
    }
    s.flush(Duration::from_secs(600)).unwrap();
    s.shutdown(Duration::from_secs(120)).unwrap();
}

fn time<T>(
    name: &str,
    runs: usize,
    mut f: impl FnMut() -> T,
    extra: impl Fn(&T) -> serde_json::Value,
) {
    let mut ms = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let out = f();
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        last = Some(out);
    }
    ms.sort_by(|a, b| a.total_cmp(b));
    let p = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    println!(
        "{}",
        json!({"bench": name, "runs": runs, "p50_ms": p(0.5), "p95_ms": p(0.95), "max_ms": p(1.0),
               "result": last.as_ref().map(&extra)})
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let n: u64 = arg("--events").map_or(2_000_000, |v| v.parse().unwrap());
    let share: f64 = arg("--agent-share").map_or(0.03, |v| v.parse().unwrap());
    let dir = arg("--dir").map_or_else(
        || std::env::temp_dir().join("tailflow-bench-stats"),
        PathBuf::from,
    );
    let path = dir.join("events.db");
    if !(args.iter().any(|a| a == "--reuse") && path.exists()) {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = Instant::now();
        build(&path, n, share);
        let stored = open(&path).status().map(|st| st.db.events).unwrap_or(0);
        println!(
            "{}",
            json!({"bench": "build", "events": n, "stored": stored, "secs": t.elapsed().as_secs_f64(),
            "db_mb": std::fs::metadata(&path).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0)})
        );
    }
    let t_open = Instant::now();
    let s = open(&path);
    println!(
        "{}",
        json!({"bench": "open", "ms": t_open.elapsed().as_secs_f64() * 1000.0,
        "note": "includes any pending schema migration (v2 → v3 builds events_agent_activity)"})
    );
    let q = |since_h: i64| StatsQuery {
        since: Utc::now() - ChronoDuration::hours(since_h),
        until: None,
        projects: Vec::new(),
        agents: Vec::new(),
        buckets: 48,
    };
    for (label, h) in [("stats_1h", 1), ("stats_24h", 24), ("stats_7d", 7 * 24)] {
        time(
            label,
            20,
            || s.stats(&q(h)).unwrap(),
            |st| {
                json!({
            "ingested": st.events_ingested, "tool_calls": st.activity.tool_calls,
            "failures": st.activity.failures, "sessions": st.sessions.total, "runtime_errors": st.runtime_errors})
            },
        );
    }
    time(
        "stats_7d_agent_project",
        20,
        || {
            s.stats(&StatsQuery {
                agents: vec!["codex".into()],
                projects: vec!["/work/web".into()],
                ..q(7 * 24)
            })
            .unwrap()
        },
        |st| json!({"tool_calls": st.activity.tool_calls}),
    );
    let fq = FilesQuery {
        range: q(7 * 24),
        sessions: Vec::new(),
        path_prefix: None,
        multi_agent: false,
        limit: 500,
    };
    time(
        "files_7d",
        20,
        || s.files(&fq).unwrap(),
        |p| json!({"paths": p.total_paths, "rows": p.files.len()}),
    );
    time(
        "files_7d_multi_agent",
        20,
        || {
            s.files(&FilesQuery {
                multi_agent: true,
                ..fq.clone()
            })
            .unwrap()
        },
        |p| json!({"paths": p.total_paths}),
    );
    time(
        "sessions_page_50",
        20,
        || {
            s.sessions(&SessionQuery {
                limit: 50,
                ..SessionQuery::default()
            })
            .unwrap()
        },
        |p| json!({"rows": p.sessions.len(), "has_more": p.has_more}),
    );
    let cursor = s
        .sessions(&SessionQuery {
            limit: 50,
            ..SessionQuery::default()
        })
        .unwrap()
        .next_cursor;
    time(
        "sessions_page_50_cursor",
        20,
        || {
            s.sessions(&SessionQuery {
                limit: 50,
                cursor: cursor.clone(),
                ..SessionQuery::default()
            })
            .unwrap()
        },
        |p| json!({"rows": p.sessions.len()}),
    );
    time(
        "sessions_status_ended",
        20,
        || {
            s.sessions(&SessionQuery {
                limit: 50,
                statuses: vec!["ended".into()],
                ..SessionQuery::default()
            })
            .unwrap()
        },
        |p| json!({"rows": p.sessions.len(), "has_more": p.has_more}),
    );
    time(
        "events_agent_page_200_desc",
        20,
        || {
            s.query(&EventQuery {
                event_types: vec!["agent.*".into(), "command.*".into(), "file.*".into()],
                order: Order::Desc,
                limit: 200,
                ..EventQuery::default()
            })
            .unwrap()
        },
        |p| json!({"rows": p.events.len()}),
    );
    time(
        "events_failures_24h",
        20,
        || {
            s.query(&EventQuery {
                event_types: vec![
                    "agent.tool.failed".into(),
                    "command.failed".into(),
                    "runtime.error".into(),
                ],
                since: Some(Utc::now() - ChronoDuration::hours(24)),
                order: Order::Desc,
                limit: 200,
                ..EventQuery::default()
            })
            .unwrap()
        },
        |p| json!({"rows": p.events.len()}),
    );
    time(
        "events_program_cargo_7d",
        20,
        || {
            s.query(&EventQuery {
                event_types: vec!["command.*".into()],
                program: Some("cargo".into()),
                order: Order::Desc,
                limit: 100,
                ..EventQuery::default()
            })
            .unwrap()
        },
        |p| json!({"rows": p.events.len()}),
    );
    s.shutdown(Duration::from_secs(60)).unwrap();
}
