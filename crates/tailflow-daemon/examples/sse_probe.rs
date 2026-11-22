//! SSE delivery probe: `sse_probe <port> <seconds>`.
//!
//! Subscribes to `/events`, parses the `t=<micros> n=<index>` prefix written
//! by `loadgen`, and prints a JSON summary: records received, distinct
//! indexes, delivery-latency percentiles (generation → SSE receipt), and
//! `gap` events.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(7878);
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(500))).ok();
    write!(
        s,
        "GET /events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let mut r = BufReader::new(s);
    let end = Instant::now() + Duration::from_secs(secs);
    let mut lat = Vec::new();
    let mut seen = HashSet::new();
    let mut received = 0u64;
    let mut gaps = 0u64;
    let mut gap_records = 0u64;
    let mut next_is_gap = false;
    let mut line = String::new();
    while Instant::now() < end {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => continue,
        }
        if line.starts_with("event: gap") {
            next_is_gap = true;
            continue;
        }
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if next_is_gap {
            next_is_gap = false;
            gaps += 1;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                gap_records += v["dropped"].as_u64().unwrap_or(0);
            }
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let payload = v["payload"].as_str().unwrap_or("");
        let mut t = None;
        let mut n = None;
        for tok in payload.split_whitespace().take(2) {
            if let Some(x) = tok.strip_prefix("t=") {
                t = x.parse::<u128>().ok();
            } else if let Some(x) = tok.strip_prefix("n=") {
                n = x.parse::<u64>().ok();
            }
        }
        if let (Some(t), Some(n)) = (t, n) {
            received += 1;
            seen.insert(n);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_micros());
            lat.push(now.saturating_sub(t) as f64 / 1000.0);
        }
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{}",
        serde_json::json!({
            "received": received,
            "distinct": seen.len(),
            "gap_events": gaps,
            "gap_records": gap_records,
            "latency_ms": {
                "p50": pct(&lat, 0.5), "p95": pct(&lat, 0.95),
                "p99": pct(&lat, 0.99), "max": lat.last(),
            }
        })
    );
}
