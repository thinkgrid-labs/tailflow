//! Fixed-rate log generator for daemon benchmarks.
//!
//! `loadgen <events_per_sec> <seconds> [burst=<on_ms>:<off_ms>] [big=<every>:<bytes>]`
//! prints a realistic mix of log lines, each prefixed `t=<unix micros>
//! n=<index>` so a probe can measure delivery latency and loss. Output is
//! flushed once per millisecond tick.
//!
//! - `burst=on:off` emits at `events_per_sec` only during `on` ms windows and
//!   stays silent for `off` ms between them (the total still equals
//!   `events_per_sec × seconds × on/(on+off)`; printed on stderr).
//! - `big=every:bytes` makes every `every`-th line a JSON object padded to
//!   about `bytes` bytes, for payload-size sensitivity runs.
//! - `linger=<secs>` stays alive that long after the last line (default 3600,
//!   so "process exited" lines do not skew a run); `linger=0` exits at once.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rate: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut burst: Option<(u64, u64)> = None;
    let mut big: Option<(u64, usize)> = None;
    let mut linger = 3600u64;
    for arg in args.iter().skip(3) {
        let (key, val) = arg.split_once('=').unwrap_or((arg.as_str(), ""));
        let pair = val
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
        if key == "linger" {
            if let Ok(v) = val.parse() {
                linger = v;
                continue;
            }
        }
        match (key, pair) {
            ("burst", Some((on, off))) if on > 0 => burst = Some((on, off)),
            ("big", Some((every, bytes))) if every > 0 => big = Some((every, bytes as usize)),
            _ => {
                eprintln!("loadgen: ignoring unrecognized argument {arg:?}");
            }
        }
    }
    // Active milliseconds in the run, so bursts emit at `rate` while on.
    let active_ms = |elapsed_ms: u64| -> u64 {
        match burst {
            None => elapsed_ms,
            Some((on, off)) => {
                let period = on + off;
                (elapsed_ms / period) * on + (elapsed_ms % period).min(on)
            }
        }
    };
    let total = rate * active_ms(secs * 1000) / 1000;
    eprintln!("loadgen: {total} lines");
    let out = std::io::stdout();
    let mut w = BufWriter::with_capacity(1 << 16, out.lock());
    let start = Instant::now();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut n = 0u64;
    while n < total {
        let elapsed_ms = start.elapsed().as_millis() as u64;
        // Past the end, emit whatever is left so exactly `total` lines go out.
        let due = if elapsed_ms >= secs * 1000 {
            total
        } else {
            active_ms(elapsed_ms) * rate / 1000
        };
        let upto = due.min(total);
        while n < upto {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_micros());
            let big_line = big.filter(|(every, _)| n.is_multiple_of(*every)).map(|(_, bytes)| {
                let pad = bytes.saturating_sub(120);
                format!(
                    r#"t={t} n={n} {{"level":"info","msg":"large payload","trace_id":"{:016x}","blob":"{}"}}"#,
                    x,
                    "x".repeat(pad)
                )
            });
            let line = if let Some(line) = big_line {
                line
            } else {
                match x % 100 {
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
                }
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
    std::thread::sleep(Duration::from_secs(linger));
}
