#!/usr/bin/env python3
"""End-to-end daemon benchmark: compare daemon binaries on identical load.

Usage:
  cargo build --release -p tailflow-daemon --examples
  python3 scripts/bench/daemon_bench.py \
      --bin baseline=/path/to/old/tailflow-daemon \
      --bin m1=target/release/tailflow-daemon \
      --bin m1-nostorage=target/release/tailflow-daemon:--no-storage \
      --rates 100,1000,10000 --seconds 20 --out results.jsonl

For each binary it measures, on the same host and with the same generator:
  idle      CPU % and RSS over --idle-seconds with a silent source
  load      CPU %, peak RSS, SSE delivery latency p50/p95/p99, records lost
            at ingestion (generated vs ring cursor), records persisted
  shutdown  SIGTERM-to-exit time

CPU % is accumulated CPU time (ps `time`) divided by wall time, so it is an
average over the window, not a sample. Every scenario starts a fresh daemon
with a fresh database directory.
"""

import argparse
import json
import os
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request

# Daemons from M4 require an API token. Every daemon this script starts
# inherits this one through the environment (older daemons ignore it).
TOKEN = os.environ.setdefault("TAILFLOW_TOKEN", "tailflow-bench-token-0123456789abcdef")
AUTH = {"Authorization": f"Bearer {TOKEN}"}

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
EXAMPLES = os.path.join(ROOT, "target", "release", "examples")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def get(port, path, timeout=5):
    try:
        with urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=AUTH), timeout=timeout) as r:
            return json.loads(r.read())
    except Exception:
        return None


def cputime(pid):
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    # [[dd-]hh:]mm:ss.ss
    days = 0
    if "-" in out:
        d, out = out.split("-", 1)
        days = int(d)
    parts = [float(p) for p in out.split(":")]
    secs = 0.0
    for p in parts:
        secs = secs * 60 + p
    return days * 86400 + secs


def rss_kb(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) if out else None


def start(binary, extra, cmd, workdir):
    port = free_port()
    cfg = os.path.join(workdir, "tailflow.toml")
    with open(cfg, "w") as f:
        f.write(
            f'[[sources.process]]\nlabel = "gen"\ncmd = "{cmd}"\n\n'
            f'[storage]\npath = "{os.path.join(workdir, "events.db")}"\n'
        )
    p = subprocess.Popen(
        [binary, "--config", cfg, "--port", str(port), *extra],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        env={**os.environ, "RUST_LOG": "error"},
    )
    deadline = time.time() + 30
    while time.time() < deadline:
        if get(port, "/health", timeout=1):
            return p, port
        time.sleep(0.05)
    p.kill()
    raise RuntimeError(f"{binary} did not start")


def stop(p):
    t = time.time()
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        return None
    return (time.time() - t) * 1000


def sample(pid, seconds):
    c0, t0 = cputime(pid), time.time()
    peak = 0
    rss = []
    while time.time() - t0 < seconds:
        r = rss_kb(pid)
        if r:
            rss.append(r)
            peak = max(peak, r)
        time.sleep(0.5)
    c1, t1 = cputime(pid), time.time()
    return {
        "cpu_percent": round(100 * (c1 - c0) / (t1 - t0), 2) if c0 is not None and c1 is not None else None,
        "rss_mb_avg": round(sum(rss) / len(rss) / 1024, 1) if rss else None,
        "rss_mb_peak": round(peak / 1024, 1),
    }


def idle(name, binary, extra, seconds):
    with tempfile.TemporaryDirectory() as d:
        p, port = start(binary, extra, "sleep 3600", d)
        time.sleep(5)  # warm-up: startup cleanup, first heartbeat
        res = sample(p.pid, seconds)
        res["shutdown_ms"] = stop(p)
        return {"binary": name, "scenario": "idle", "seconds": seconds, **res}


def load(name, binary, extra, rate, seconds):
    loadgen = os.path.join(EXAMPLES, "loadgen")
    probe = os.path.join(EXAMPLES, "sse_probe")
    with tempfile.TemporaryDirectory() as d:
        # Generator starts 2 s after the daemon so the probe is subscribed.
        p, port = start(binary, extra, f"sleep 2; exec {loadgen} {rate} {seconds}", d)
        pr = subprocess.Popen([probe, str(port), str(seconds + 4)], stdout=subprocess.PIPE, text=True)
        time.sleep(2)
        res = sample(p.pid, seconds)
        time.sleep(2)  # drain
        out, _ = pr.communicate(timeout=seconds + 30)
        sse = json.loads(out) if out.strip() else {}
        health = get(port, "/health") or {}
        status = get(port, "/api/v1/storage/status") or {}
        generated = rate * seconds
        ingested = health.get("cursor", 0)
        res.update({
            "rate": rate,
            "generated": generated,
            "ingested_ring_cursor": ingested,
            "ingest_dropped_reported": health.get("ingest_dropped"),
            "sse": sse,
            "sse_lost": generated - sse.get("distinct", 0) if sse else None,
            "persisted": (status.get("health") or {}).get("events_persisted"),
            "storage_dropped": (status.get("health") or {}).get("events_dropped_total"),
            "db_bytes": ((status.get("db") or {}).get("file_bytes", 0) + (status.get("db") or {}).get("wal_bytes", 0)) or None,
        })
        res["shutdown_ms"] = stop(p)
        return {"binary": name, "scenario": f"load_{rate}", "seconds": seconds, **res}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", action="append", required=True, help="name=path[:extra args]")
    ap.add_argument("--rates", default="100,1000,10000")
    ap.add_argument("--seconds", type=int, default=20)
    ap.add_argument("--idle-seconds", type=int, default=30)
    ap.add_argument("--out", default="-")
    a = ap.parse_args()
    bins = []
    for b in a.bin:
        name, rest = b.split("=", 1)
        path, _, extra = rest.partition(":")
        bins.append((name, os.path.abspath(path), extra.split() if extra else []))
    out = open(a.out, "w") if a.out != "-" else None
    def emit(r):
        line = json.dumps(r)
        print(line, flush=True)
        if out:
            out.write(line + "\n")
            out.flush()
    for name, path, extra in bins:
        emit(idle(name, path, extra, a.idle_seconds))
    for rate in [int(r) for r in a.rates.split(",")]:
        # Interleave binaries per rate so drift in host load affects all alike.
        for name, path, extra in bins:
            emit(load(name, path, extra, rate, a.seconds))


if __name__ == "__main__":
    main()
