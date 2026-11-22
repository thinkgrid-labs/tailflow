//! Fixed-rate log generator for daemon benchmarks.
//!
//! `loadgen <events_per_sec> <seconds>` prints a realistic mix of log lines,
//! each prefixed `t=<unix micros> n=<index>` so a probe can measure delivery
//! latency and loss. Output is flushed once per millisecond tick.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rate: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let total = rate * secs;
    let out = std::io::stdout();
    let mut w = BufWriter::with_capacity(1 << 16, out.lock());
    let start = Instant::now();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut n = 0u64;
    while n < total {
        let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
        let upto = due.min(total);
        while n < upto {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_micros());
            let line = match x % 100 {
                0..=54 => format!(
                    "t={t} n={n} GET /api/users/{} 200 {}ms ua=Mozilla/5.0 req={:08x}",
                    x % 1000,
                    x % 400,
                    x as u32
                ),
                55..=79 => format!(
                    "t={t} n={n} INFO processed job id={n} in {}ms queue=default attempts=1",
                    x % 900
                ),
                80..=91 => format!(
                    r#"t={t} n={n} {{"level":"info","msg":"request completed","path":"/api/orders","status":201,"duration_ms":{},"trace_id":"{:016x}"}}"#,
                    x % 300,
                    x
                ),
                92..=96 => format!("t={t} n={n} WARN slow query took {}ms", 500 + x % 4000),
                97..=98 => format!(
                    "t={t} n={n} ERROR connection refused: postgres:5432 password=hunter{}",
                    x % 100
                ),
                _ => format!("t={t} n={n}     at Pool.connect (index.js:{}:9)", x % 400),
            };
            if writeln!(w, "{line}").is_err() {
                return;
            }
            n += 1;
        }
        if w.flush().is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = w.flush();
    // Stay alive so "process exited" lines do not skew the end of the run.
    std::thread::sleep(Duration::from_secs(3600));
}
