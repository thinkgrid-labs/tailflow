//! Per-stage CPU cost of the live ingestion pipeline.
//!
//! `cargo run --release -p tailflow-core --example bench_pipeline [rounds]`
//!
//! Every stage a runtime line passes through between a collector and the
//! storage queue, measured on the same deterministic corpus `loadgen` emits
//! (55 % access lines, 25 % app lines, 12 % JSON, 5 % warn, 2 % error with a
//! secret, 1 % stack frames). Reports the median ns per line over `rounds`
//! rounds, plus allocation-free baselines for scale. Uses only APIs that
//! exist since M1 so the same file measures older revisions.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::hint::black_box;
use std::time::Instant;
use tailflow_core::{
    event::ObservabilityEvent,
    privacy::{CapturePolicy, LineRedactor, PrivacyConfig},
    query::{fingerprint, LogStore, Query},
    LogLevel, LogRecord,
};

fn corpus(n: usize) -> Vec<String> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..n)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let t = 1_700_000_000_000_000u64 + i as u64 * 1000;
            match x % 100 {
                0..=54 => format!(
                    "t={t} n={i} GET /api/users/{} 200 {}ms ua=Mozilla/5.0 req={:08x}",
                    x % 1000,
                    x % 400,
                    x as u32
                ),
                55..=79 => format!(
                    "t={t} n={i} INFO processed job id={i} in {}ms queue=default attempts=1",
                    x % 900
                ),
                80..=91 => format!(
                    r#"t={t} n={i} {{"level":"info","msg":"request completed","path":"/api/orders","status":201,"duration_ms":{},"trace_id":"{:016x}"}}"#,
                    x % 300,
                    x
                ),
                92..=96 => format!("t={t} n={i} WARN slow query took {}ms", 500 + x % 4000),
                97..=98 => format!(
                    "t={t} n={i} ERROR connection refused: postgres:5432 password=hunter{}",
                    x % 100
                ),
                _ => format!("t={t} n={i}     at Pool.connect (index.js:{}:9)", x % 400),
            }
        })
        .collect()
}

fn records(lines: &[String]) -> Vec<LogRecord> {
    let now = chrono::Utc::now();
    lines
        .iter()
        .map(|l| LogRecord {
            timestamp: now,
            source: "api".into(),
            level: LogLevel::detect(l),
            payload: l.clone(),
        })
        .collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Median ns per item of `f` applied to `n` items, over `rounds` rounds.
/// `setup` runs untimed before each round.
fn time_per_item<T, S, F>(rounds: usize, n: usize, mut setup: S, mut f: F) -> f64
where
    S: FnMut() -> T,
    F: FnMut(T),
{
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let input = setup();
        let t = Instant::now();
        f(input);
        samples.push(t.elapsed().as_nanos() as f64 / n as f64);
    }
    median(samples)
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(9);
    let n = 20_000;
    let lines = corpus(n);
    let recs = records(&lines);
    let policy = CapturePolicy::from_config(&PrivacyConfig::default()).expect("default policy");
    let mut out: Vec<(&str, f64)> = Vec::new();

    out.push((
        "baseline: clone payload String",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                for l in &lines {
                    black_box(l.clone());
                }
            },
        ),
    ));
    out.push((
        "LogLevel::detect",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                for l in &lines {
                    black_box(LogLevel::detect(black_box(l)));
                }
            },
        ),
    ));
    out.push((
        "LineRedactor::redact_line (fan-out)",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                let mut r = LineRedactor::new(policy.redactor());
                for l in &lines {
                    black_box(r.redact_line("api", black_box(l)));
                }
            },
        ),
    ));
    // The storage-side pass sees lines the fan-out already redacted.
    let clean: Vec<String> = {
        let mut r = LineRedactor::new(policy.redactor());
        lines
            .iter()
            .map(|l| {
                r.redact_line("api", l)
                    .map_or_else(|| l.clone(), |(c, _)| c)
            })
            .collect()
    };
    out.push((
        "Redactor::redact on clean line (storage pass)",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                for l in &clean {
                    black_box(policy.redactor().redact(black_box(l)));
                }
            },
        ),
    ));
    let store = LogStore::new(5_000);
    out.push((
        "LogStore::push (ring 5000, incl. record move)",
        time_per_item(
            rounds,
            n,
            || recs.clone(),
            |batch: Vec<LogRecord>| {
                for r in batch {
                    black_box(store.push(r));
                }
            },
        ),
    ));
    out.push((
        "storage twin: from_log_record + admit + sanitize + validate",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                for (i, r) in recs.iter().enumerate() {
                    let mut e = ObservabilityEvent::from_log_record(
                        r,
                        format!("run:{i}"),
                        "process",
                        Some(i as u64),
                    );
                    let _ = black_box(policy.admit(&e));
                    policy.sanitize(&mut e);
                    let _ = black_box(e.validate());
                    black_box(e);
                }
            },
        ),
    ));
    let filled = LogStore::new(5_000);
    let mut last = None;
    for r in recs.iter().take(5_000) {
        last = Some(filled.push(r.clone()));
    }
    let sample = last.expect("pushed");
    // `push` returns `SeqRecord` (M1) or `Arc<SeqRecord>` (M1.5); coerce both.
    let sample: &tailflow_core::query::SeqRecord = &sample;
    out.push((
        "serde_json::to_string(SeqRecord) (per SSE client)",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                for _ in 0..n {
                    black_box(serde_json::to_string(black_box(sample)).expect("serialize"));
                }
            },
        ),
    ));
    out.push((
        "fingerprint",
        time_per_item(
            rounds,
            n,
            || (),
            |()| {
                for l in &lines {
                    black_box(fingerprint(black_box(l)));
                }
            },
        ),
    ));

    println!("{:<62} {:>10}", "stage (per line)", "ns");
    for (name, ns) in &out {
        println!("{name:<62} {ns:>10.0}");
    }

    // Whole-ring query costs (one call each, 5000 records).
    let q_err =
        Query::new(tailflow_core::processor::Filter::none().with_min_level(LogLevel::Error));
    let calls: Vec<(&str, f64)> = vec![
        (
            "summarize(level>=error) over 5000",
            time_per_item(
                rounds,
                1,
                || (),
                |()| {
                    black_box(filled.summarize(&q_err, 8));
                },
            ),
        ),
        (
            "summarize(all levels) over 5000",
            time_per_item(
                rounds,
                1,
                || (),
                |()| {
                    black_box(filled.summarize(&Query::default(), 8));
                },
            ),
        ),
        (
            "records(limit 100) over 5000",
            time_per_item(
                rounds,
                1,
                || (),
                |()| {
                    black_box(filled.records(&Query::default()));
                },
            ),
        ),
        (
            "records(all, no elision) over 5000 (/api/records)",
            time_per_item(
                rounds,
                1,
                || (),
                |()| {
                    black_box(
                        filled.records(
                            &Query::default()
                                .with_limit(usize::MAX)
                                .with_max_payload_chars(usize::MAX),
                        ),
                    );
                },
            ),
        ),
        (
            "sources() over 5000",
            time_per_item(
                rounds,
                1,
                || (),
                |()| {
                    black_box(filled.sources());
                },
            ),
        ),
    ];
    println!("\n{:<62} {:>10}", "query (per call)", "µs");
    for (name, ns) in &calls {
        println!("{name:<62} {:>10.1}", ns / 1000.0);
    }
}
