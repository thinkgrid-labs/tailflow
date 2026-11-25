#!/usr/bin/env python3
"""Resource benchmark for tailflow-daemon (M1.5).

Compares daemon binaries on identical workloads and reports per-process CPU,
wakeups, resident and private memory, bytes written to disk, throughput,
SSE delivery latency, query latency, and loss. Every scenario starts a fresh
daemon with a fresh database directory unless it says otherwise.

  cargo build --release -p tailflow-daemon --bin tailflow-daemon --examples
  python3 scripts/bench/resource_bench.py \\
      --bin baseline=/path/to/old/tailflow-daemon \\
      --bin new=target/release/tailflow-daemon \\
      --scenarios idle,load:100,load:1000,burst,mixed --repeat 3 \\
      --out results.jsonl

Scenarios (comma separated; `name:arg` where noted):
  idle[:seconds]          silent process source (default 60 s)
  idle_file[:seconds]     one idle tailed file
  startup                 spawn → /health → first /api/v1/events answer
  load:<rate>[:seconds[:quiet]]
                          steady rate (default 20 s), then `quiet` s (default
                          10) without input to see whether memory returns
  big:<rate>[:bytes[:every[:seconds]]]
                          steady rate where every `every`-th line (default 10)
                          is ~bytes large (default 65536), for 20 s default
  burst[:rate]            on/off bursts (500 ms on / 1500 ms off) at rate
                          (default 10000/s) for 20 s
  fanout:<clients>        1000/s with N concurrent SSE clients
  mixed[:seconds]         1000/s + 2 SSE clients + a query loop + retention
                          pressure (max_size_mb = 16) for 60 s
  querymem                memory before/after a 4-thread query burst against a
                          copy of $TAILFLOW_BENCH_DB (a large existing DB),
                          and again after 30 s quiet
  walamp:<rate>[:seconds[:batch_delay_ms]]
                          SQLite write amplification and v1 visibility. A read
                          transaction held open pins the WAL so it never
                          resets; its final size is then every page frame the
                          writer appended. Also polls /api/v1/events with
                          `after` to measure generation → queryable latency.
                          OS per-process disk counters miss delayed
                          write-back on macOS, so this is the exact metric.
  endurance:<minutes>     cycling rates (idle, 10, 100, 1000/s) with periodic
                          queries and retention; RSS sampled every 10 s
  soak:<minutes>[:max_size_mb]
                          multi-hour stability run: endurance cycle plus a
                          10k/s burst, reconnecting SSE clients, heavy history
                          and common-term FTS reads, retention at max_size_mb
                          (default 32). Full time series (memory, WAL, DB,
                          loss, retention passes, per-query latency, macOS
                          malloc zone, FTS segment size). Analyse with
                          soak_report.py. TAILFLOW_BENCH_KEEP=<path> keeps the
                          final database.

--repeat N runs every (scenario, binary) pair N times, interleaving binaries
so host drift affects both alike; summaries report median, min, max and
relative spread. Raw runs are kept in the JSONL output.
"""

import argparse
import json
import os
import platform
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import procstat  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
EXAMPLES = os.environ.get("TAILFLOW_BENCH_EXAMPLES", os.path.join(ROOT, "target", "release", "examples"))
MB = 1024 * 1024

# Fault injection for the CI gate's self-test (`check_budgets.py --inject`).
# Never set in benchmarks. crash: SIGKILL the daemon mid-run; probe: the SSE
# probe dies without output; probe_garbage: it prints malformed output; loss:
# a 256-event storage queue plus a held write lock make storage really drop
# events; missing: the storage status endpoint "returns" nothing.
FAULT = None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def get(port, path, timeout=5):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=timeout) as r:
            return json.loads(r.read())
    except Exception:
        return None


def timed_get(port, path, timeout=10):
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=timeout) as r:
            r.read()
            ok = r.status == 200
    except Exception:
        ok = False
    return (time.perf_counter() - t) * 1000, ok


def write_config(workdir, cmd=None, file_path=None, storage_extra=""):
    cfg = os.path.join(workdir, "tailflow.toml")
    with open(cfg, "w") as f:
        if cmd is not None:
            f.write(f'[[sources.process]]\nlabel = "gen"\ncmd = "{cmd}"\n\n')
        if file_path is not None:
            f.write(f'[[sources.file]]\npath = "{file_path}"\nlabel = "file"\n\n')
        if FAULT == "loss":
            storage_extra += "queue_capacity = 256\n"
        f.write(f'[storage]\npath = "{os.path.join(workdir, "events.db")}"\n{storage_extra}')
    return cfg


def start(binary, extra, workdir, cfg):
    port = free_port()
    t0 = time.perf_counter()
    p = subprocess.Popen(
        [binary, "--config", cfg, "--port", str(port), *extra],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        # TAILFLOW_DATA_DIR: from M2 a daemon ingests the per-user agent
        # spool; a benchmark must never touch the developer's real one.
        env={**os.environ, "RUST_LOG": "error", "TAILFLOW_DATA_DIR": workdir}, cwd=workdir,
    )
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                pass
        except OSError:
            time.sleep(0.002)
            continue
        if get(port, "/health", timeout=1):
            _inject_runtime_fault(p, workdir)
            return p, port, (time.perf_counter() - t0) * 1000
        time.sleep(0.002)
    p.kill()
    raise RuntimeError(f"{binary} did not start")


def _inject_runtime_fault(p, workdir):
    if FAULT == "crash":
        threading.Timer(6.0, p.kill).start()
    elif FAULT == "loss":
        def hold_lock():
            import sqlite3
            time.sleep(4.0)  # load starts ~2 s after the daemon
            c = sqlite3.connect(os.path.join(workdir, "events.db"), isolation_level=None, timeout=10)
            c.execute("BEGIN IMMEDIATE")
            time.sleep(6.0)  # > the writer's 5 s busy timeout, queue overflows
            c.execute("COMMIT")
            c.close()
        threading.Thread(target=hold_lock, daemon=True).start()


def stop(p):
    """SIGTERM and wait. Returns ms, or None if it had to be killed or did
    not exit cleanly (non-zero status) — callers in CI treat None as failure."""
    t = time.time()
    if p.poll() is not None:
        return None
    p.send_signal(signal.SIGTERM)
    try:
        code = p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        return None
    if code != 0:
        return None
    return round((time.time() - t) * 1000, 1)


class Sampler:
    """Polls RSS/private memory every `every` s; computes counter deltas."""

    def __init__(self, pid, every=0.5):
        self.pid, self.every = pid, every
        self.rss, self.private = [], []
        self._stop = threading.Event()
        self.first = procstat.snapshot(pid)
        self.t0 = time.time()
        self._t = threading.Thread(target=self._run, daemon=True)
        self._t.start()

    def _run(self):
        while not self._stop.wait(self.every):
            s = procstat.snapshot(self.pid)
            if s:
                self.rss.append(s["rss_bytes"])
                if s.get("private_bytes") is not None:
                    self.private.append(s["private_bytes"])

    def finish(self):
        self._stop.set()
        self._t.join()
        last = procstat.snapshot(self.pid)
        wall = time.time() - self.t0
        d = procstat.delta(self.first, last) or {}
        r = lambda v, n=2: None if v is None else round(v, n)  # noqa: E731
        return {
            "wall_s": r(wall, 1),
            "cpu_percent": r(100 * d["cpu_s"] / wall, 3) if d.get("cpu_s") is not None else None,
            "wakeups_per_s": r(d["wakeups"] / wall, 2) if d.get("wakeups") is not None else None,
            "disk_write_kb_per_s": r(d["disk_write_bytes"] / 1024 / wall, 2) if d.get("disk_write_bytes") is not None else None,
            "disk_write_bytes": d.get("disk_write_bytes"),
            "rss_mb_avg": r(statistics.mean(self.rss) / MB, 1) if self.rss else None,
            "rss_mb_peak": r(max(self.rss) / MB, 1) if self.rss else None,
            "private_mb_avg": r(statistics.mean(self.private) / MB, 1) if self.private else None,
            "private_mb_peak": r(max(self.private) / MB, 1) if self.private else None,
            "rss_mb_end": r(last["rss_bytes"] / MB, 1) if last else None,
            "private_mb_end": r(last["private_bytes"] / MB, 1) if last and last.get("private_bytes") is not None else None,
        }


def storage_status(port):
    status = {} if FAULT == "missing" else (get(port, "/api/v1/storage/status") or {})
    h = status.get("health") or {}
    db = status.get("db") or {}
    return {
        "persisted": h.get("events_persisted"),
        "storage_dropped": h.get("events_dropped_total"),
        "db_bytes": (db.get("file_bytes", 0) + db.get("wal_bytes", 0)) or None,
    }


def probe(port, seconds):
    if FAULT == "probe":
        return subprocess.Popen([sys.executable, "-c", "import sys; sys.exit(3)"], stdout=subprocess.PIPE, text=True)
    if FAULT == "probe_garbage":
        return subprocess.Popen([sys.executable, "-c", "print('{not json')"], stdout=subprocess.PIPE, text=True)
    return subprocess.Popen([os.path.join(EXAMPLES, "sse_probe"), str(port), str(seconds)],
                            stdout=subprocess.PIPE, text=True)


def probe_result(pr, timeout):
    """Parsed probe summary. A probe that failed or printed something
    unparseable yields {} — every consumer treats that as missing data."""
    out, _ = pr.communicate(timeout=timeout)
    if pr.returncode != 0:
        return {}
    try:
        r = json.loads(out) if out.strip() else {}
    except ValueError:
        return {}
    if not isinstance(r, dict):
        return {}
    lat = r.get("latency_ms") or {}
    for k in ("p50", "p95", "p99", "max"):
        r[f"{k}_ms"] = lat.get(k)
    return r


def pct(v, p):
    if not v:
        return None
    v = sorted(v)
    return round(v[min(len(v) - 1, int(round((len(v) - 1) * p)))], 2)


QUERIES = [
    "/api/v1/events?limit=100",
    "/api/v1/events?type=runtime.error&since=1h&limit=100",
    "/api/v1/events?q=connection%20refused&limit=50",
    "/api/v1/events?source=gen&severity=warn&limit=100",
    "/api/errors",
    "/api/query?level=warn&limit=100",
    "/api/sources",
]


class QueryLoop:
    def __init__(self, port, interval=0.2):
        self.port, self.interval = port, interval
        self.lat, self.failed = [], 0
        self._stop = threading.Event()
        self._t = threading.Thread(target=self._run, daemon=True)
        self._t.start()

    def _run(self):
        i = 0
        while not self._stop.wait(self.interval):
            ms, ok = timed_get(self.port, QUERIES[i % len(QUERIES)])
            i += 1
            if ok:
                self.lat.append(ms)
            else:
                self.failed += 1

    def finish(self):
        self._stop.set()
        self._t.join()
        return {"queries": len(self.lat), "query_failed": self.failed,
                "query_p50_ms": pct(self.lat, 0.5), "query_p95_ms": pct(self.lat, 0.95),
                "query_p99_ms": pct(self.lat, 0.99), "query_max_ms": pct(self.lat, 1.0)}


def gen_cmd(rate, seconds, *extra, delay=2):
    loadgen = os.path.join(EXAMPLES, "loadgen")
    args = " ".join(extra)
    return f"sleep {delay}; exec {loadgen} {rate} {seconds} {args}".strip()


# ── Scenarios ─────────────────────────────────────────────────────────────────

def sc_idle(binary, extra, seconds=60, file_source=False):
    with tempfile.TemporaryDirectory() as d:
        if file_source:
            fp = os.path.join(d, "idle.log")
            open(fp, "w").close()
            cfg = write_config(d, file_path=fp)
        else:
            cfg = write_config(d, cmd="sleep 86400")
        p, port, _ = start(binary, extra, d, cfg)
        time.sleep(5)  # startup work, first heartbeat
        s = Sampler(p.pid, every=1.0)
        time.sleep(seconds)
        res = s.finish()
        res["daemon_alive"] = p.poll() is None
        res["shutdown_ms"] = stop(p)
        return res


def sc_startup(binary, extra):
    with tempfile.TemporaryDirectory() as d:
        cfg = write_config(d, cmd="sleep 86400")
        out = {}
        # Cold: new database. Warm: reopen the same database.
        for phase in ("fresh_db", "existing_db"):
            p, port, health_ms = start(binary, extra, d, cfg)
            t = time.perf_counter()
            while True:
                ms, ok = timed_get(port, "/api/v1/events?limit=10", timeout=2)
                if ok:
                    break
                if time.perf_counter() - t > 10:
                    break
            out[f"{phase}_health_ms"] = round(health_ms, 1)
            out[f"{phase}_first_query_ms"] = round(health_ms + (time.perf_counter() - t) * 1000, 1)
            snap = procstat.snapshot(p.pid)
            out[f"{phase}_rss_mb"] = round(snap["rss_bytes"] / MB, 1) if snap else None
            out[f"{phase}_shutdown_ms"] = stop(p)
        return out


def sc_load(binary, extra, rate, seconds=20, big=None, sse_clients=1, quiet=10, big_every=10):
    with tempfile.TemporaryDirectory() as d:
        gen_extra = [f"big={big_every}:{big}"] if big else []
        cfg = write_config(d, cmd=gen_cmd(rate, seconds, *gen_extra))
        p, port, _ = start(binary, extra, d, cfg)
        probes = [probe(port, seconds + 6) for _ in range(sse_clients)]
        time.sleep(2)
        s = Sampler(p.pid)
        time.sleep(seconds + 1)
        res = s.finish()
        time.sleep(3)
        sse = [probe_result(pr, seconds + 40) for pr in probes]
        alive = p.poll() is None
        health = get(port, "/health") or {}
        st = storage_status(port)
        generated = rate * seconds
        # Quiet period: does memory come back down once ingestion stops?
        q = Sampler(p.pid, every=1.0)
        time.sleep(quiet)
        quiet_res = q.finish()
        res.update({
            "rate": rate, "generated": generated, "daemon_alive": alive,
            "sse_received_min": min((x.get("received", 0) for x in sse), default=None),
            "ingested_ring_cursor": health.get("cursor"),
            "ingest_dropped_reported": health.get("ingest_dropped"),
            "sse_lost_max": max((generated - x.get("distinct", 0)) for x in sse) if sse and all(sse) else None,
            "sse_p50_ms": sse[0].get("p50_ms") if sse and sse[0] else None,
            "sse_p95_ms": sse[0].get("p95_ms") if sse and sse[0] else None,
            "sse_p99_ms": sse[0].get("p99_ms") if sse and sse[0] else None,
            **st,
            "bytes_written_per_event": round(res["disk_write_bytes"] / generated, 1) if res.get("disk_write_bytes") is not None else None,
            "quiet_rss_mb_end": quiet_res["rss_mb_end"],
            "quiet_private_mb_end": quiet_res["private_mb_end"],
            "quiet_cpu_percent": quiet_res["cpu_percent"],
        })
        res["shutdown_ms"] = stop(p)
        return res


def sc_burst(binary, extra, rate=10000, seconds=20):
    with tempfile.TemporaryDirectory() as d:
        cfg = write_config(d, cmd=gen_cmd(rate, seconds, "burst=500:1500"))
        p, port, _ = start(binary, extra, d, cfg)
        pr = probe(port, seconds + 6)
        time.sleep(2)
        s = Sampler(p.pid, every=0.25)
        time.sleep(seconds + 1)
        res = s.finish()
        time.sleep(3)
        sse = probe_result(pr, seconds + 40)
        alive = p.poll() is None
        health = get(port, "/health") or {}
        st = storage_status(port)
        generated = rate * seconds * 500 // 2000
        res.update({
            "rate": rate, "generated": generated, "daemon_alive": alive,
            "sse_received_min": sse.get("received") if sse else None,
            "ingested_ring_cursor": health.get("cursor"),
            "ingest_dropped_reported": health.get("ingest_dropped"),
            "sse_lost": generated - sse.get("distinct", 0) if sse else None,
            "sse_gap_events": sse.get("gap_events"),
            "sse_p99_ms": sse.get("p99_ms"),
            **st,
        })
        res["shutdown_ms"] = stop(p)
        return res


def sc_mixed(binary, extra, seconds=60):
    with tempfile.TemporaryDirectory() as d:
        cfg = write_config(d, cmd=gen_cmd(1000, seconds),
                           storage_extra="max_size_mb = 16\ncleanup_interval_secs = 10\n")
        p, port, _ = start(binary, extra, d, cfg)
        probes = [probe(port, seconds + 6) for _ in range(2)]
        time.sleep(2)
        s = Sampler(p.pid)
        ql = QueryLoop(port, interval=0.1)
        time.sleep(seconds + 1)
        qres = ql.finish()
        res = s.finish()
        time.sleep(3)
        sse = [probe_result(pr, seconds + 40) for pr in probes]
        health = get(port, "/health") or {}
        st = storage_status(port)
        retention = get(port, "/api/v1/storage/retention") or {}
        generated = 1000 * seconds
        res.update(qres)
        res.update({
            "generated": generated,
            "ingested_ring_cursor": health.get("cursor"),
            "ingest_dropped_reported": health.get("ingest_dropped"),
            "sse_lost_max": max((generated - x.get("distinct", 0)) for x in sse) if all(sse) else None,
            "sse_p99_ms": sse[0].get("p99_ms") if sse[0] else None,
            **st,
            "retention_last": retention.get("last_run") or retention.get("last"),
        })
        res["shutdown_ms"] = stop(p)
        return res


def sc_querymem(binary, extra, queries_per_thread=60, threads=4):
    import shutil
    src = os.environ.get("TAILFLOW_BENCH_DB")
    if not src:
        raise SystemExit("querymem needs TAILFLOW_BENCH_DB=<path to a populated events.db>")
    with tempfile.TemporaryDirectory(dir=os.environ.get("TAILFLOW_BENCH_TMP")) as d:
        shutil.copy(src, os.path.join(d, "events.db"))
        cfg = write_config(d, cmd="sleep 86400", storage_extra="max_size_mb = 1048576\nretention_days = \"unlimited\"\n")
        p, port, _ = start(binary, extra, d, cfg)
        time.sleep(3)
        before = procstat.snapshot(p.pid)
        heavy = [
            "/api/v1/events?limit=500",
            "/api/v1/events?type=runtime.error&since=7d&limit=500",
            "/api/v1/events?q=connection%20refused&limit=500",
            "/api/v1/events?q=timeout&order=desc&limit=500",
            "/api/v1/events?severity=error&since=30d&limit=500",
            "/api/v1/events?source=api&severity=warn&since=30d&limit=500",
            "/api/v1/events?type=file.modified&since=30d&limit=500",
            "/api/v1/events?after=1&limit=500",
        ]
        lat = []
        lock = threading.Lock()

        def worker(k):
            for i in range(queries_per_thread):
                ms, ok = timed_get(port, heavy[(i + k) % len(heavy)], timeout=30)
                with lock:
                    lat.append(ms if ok else None)

        ts = [threading.Thread(target=worker, args=(k,)) for k in range(threads)]
        peak = [0]
        stop_ev = threading.Event()

        def watch():
            while not stop_ev.wait(0.2):
                s_ = procstat.snapshot(p.pid)
                if s_:
                    peak[0] = max(peak[0], s_["rss_bytes"])

        w = threading.Thread(target=watch, daemon=True)
        w.start()
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        stop_ev.set()
        w.join()
        after = procstat.snapshot(p.pid)
        time.sleep(30)
        later = procstat.snapshot(p.pid)
        ok = [x for x in lat if x is not None]
        res = {
            "queries": len(lat), "query_failed": len(lat) - len(ok),
            "query_p50_ms": pct(ok, 0.5), "query_p95_ms": pct(ok, 0.95), "query_p99_ms": pct(ok, 0.99),
            "rss_mb_before": round(before["rss_bytes"] / MB, 1),
            "rss_mb_peak": round(peak[0] / MB, 1),
            "rss_mb_after": round(after["rss_bytes"] / MB, 1),
            "rss_mb_after_30s": round(later["rss_bytes"] / MB, 1),
            "private_mb_before": round((before.get("private_bytes") or 0) / MB, 1),
            "private_mb_after": round((after.get("private_bytes") or 0) / MB, 1),
            "private_mb_after_30s": round((later.get("private_bytes") or 0) / MB, 1),
        }
        res["shutdown_ms"] = stop(p)
        return res


def sc_walamp(binary, extra, rate, seconds=30, delay_ms=None):
    import sqlite3
    extra_cfg = f"batch_max_delay_ms = {delay_ms}\n" if delay_ms else ""
    with tempfile.TemporaryDirectory() as d:
        cfg = write_config(d, cmd=gen_cmd(rate, seconds, delay=3), storage_extra=extra_cfg)
        p, port, _ = start(binary, extra, d, cfg)
        db = os.path.join(d, "events.db")
        wal = db + "-wal"
        # Wait for the schema, then pin a read snapshot.
        pin = sqlite3.connect(db, isolation_level=None, timeout=10)
        # Experiment knob: drop an index first to measure its write cost.
        drop = os.environ.get("TAILFLOW_BENCH_DROP_INDEX")
        if drop:
            pin.execute(f"DROP INDEX IF EXISTS {drop}")
        pin.execute("BEGIN")
        pin.execute("SELECT count(*) FROM meta").fetchall()
        wal0 = os.path.getsize(wal) if os.path.exists(wal) else 0
        st0 = storage_status(port)
        lat = []
        after = 0
        t_end = time.time() + seconds + 6
        while time.time() < t_end:
            body = get(port, f"/api/v1/events?after={after}&limit=1000&source=gen") or {}
            now_us = time.time() * 1e6
            for e in body.get("events", []):
                after = max(after, e.get("seq", 0))
                m = e.get("message") or ""
                if m.startswith("t="):
                    try:
                        lat.append((now_us - int(m[2:m.index(" ")])) / 1000)
                    except ValueError:
                        pass
            time.sleep(0.02)
        st1 = storage_status(port)
        wal1 = os.path.getsize(wal)
        pin.execute("COMMIT")
        pin.close()
        events = (st1.get("persisted") or 0) - (st0.get("persisted") or 0)
        page = 4096
        frames = max(0, wal1 - max(wal0, 32)) / (page + 24)
        res = {
            "rate": rate, "batch_delay_ms": delay_ms or "default",
            "persisted": events,
            "wal_frames": int(frames),
            "wal_bytes_per_event": round((wal1 - max(wal0, 32)) / events, 1) if events else None,
            "frames_per_event": round(frames / events, 3) if events else None,
            "visible_events": len(lat),
            "visible_p50_ms": pct(lat, 0.5), "visible_p95_ms": pct(lat, 0.95),
            "visible_p99_ms": pct(lat, 0.99), "visible_max_ms": pct(lat, 1.0),
        }
        res["shutdown_ms"] = stop(p)
        return res


def sc_endurance(binary, extra, minutes):
    """Cycle: 60 s idle, 60 s @10/s, 60 s @100/s, 30 s @1000/s, repeat."""
    cycle = [(0, 60), (10, 60), (100, 60), (1000, 30)]
    period = sum(s for _, s in cycle)
    cycles = max(1, int(minutes * 60 // period))
    script_lines = []
    loadgen = os.path.join(EXAMPLES, "loadgen")
    for _ in range(cycles):
        for rate, secs in cycle:
            script_lines.append(f"sleep {secs}" if rate == 0 else f"{loadgen} {rate} {secs} linger=0")
    with tempfile.TemporaryDirectory() as d:
        sh = os.path.join(d, "cycle.sh")
        with open(sh, "w") as f:
            f.write("#!/bin/sh\n" + "\n".join(script_lines) + "\nsleep 86400\n")
        os.chmod(sh, 0o755)
        cfg = write_config(d, cmd=sh, storage_extra="max_size_mb = 32\ncleanup_interval_secs = 30\n")
        p, port, _ = start(binary, extra, d, cfg)
        ql = QueryLoop(port, interval=2.0)
        series = []
        first = procstat.snapshot(p.pid)
        t0 = time.time()
        end = t0 + cycles * period
        while time.time() < end:
            time.sleep(10)
            snap = procstat.snapshot(p.pid)
            if snap:
                series.append((round(time.time() - t0), round(snap["rss_bytes"] / MB, 2),
                               round((snap.get("private_bytes") or 0) / MB, 2)))
        last = procstat.snapshot(p.pid)
        qres = ql.finish()
        health = get(port, "/health") or {}
        st = storage_status(port)
        wall = time.time() - t0
        d_ = procstat.delta(first, last)
        # Memory trend over the second half (after caches have warmed).
        half = [x for x in series if x[0] >= wall / 2]
        slope = None
        if len(half) >= 3:
            xs = [x[0] / 3600 for x in half]
            ys = [x[1] for x in half]
            mx, my = statistics.mean(xs), statistics.mean(ys)
            den = sum((x - mx) ** 2 for x in xs)
            slope = round(sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den, 3) if den else None
        expected = cycles * sum(r * s for r, s in cycle)
        res = {
            "minutes": round(wall / 60, 1), "cycles": cycles, "generated": expected,
            "ingested_ring_cursor": health.get("cursor"),
            "ingest_dropped_reported": health.get("ingest_dropped"),
            "cpu_percent": round(100 * d_["cpu_s"] / wall, 3),
            "wakeups_per_s": round(d_["wakeups"] / wall, 2) if d_.get("wakeups") is not None else None,
            "disk_write_mb": round(d_["disk_write_bytes"] / MB, 1) if d_.get("disk_write_bytes") is not None else None,
            "rss_mb_min": min(x[1] for x in series) if series else None,
            "rss_mb_max": max(x[1] for x in series) if series else None,
            "rss_mb_end": series[-1][1] if series else None,
            "private_mb_max": max(x[2] for x in series) if series else None,
            "rss_slope_mb_per_hour_2nd_half": slope,
            **qres, **st,
            "series": series,
        }
        res["shutdown_ms"] = stop(p)
        return res


SOAK_QUERIES = [
    ("recent", "/api/v1/events?limit=100"),
    ("type", "/api/v1/events?type=runtime.error&since=1h&limit=100"),
    ("fts", "/api/v1/events?q=connection%20refused&limit=50"),
    ("source", "/api/v1/events?source=gen&severity=warn&limit=100"),
    ("legacy", "/api/errors"),
    ("legacy", "/api/query?level=warn&limit=100"),
    ("legacy", "/api/sources"),
]
# Heavier history reads, one every 30 s: common-term FTS (many matches in a
# tombstone-heavy index), wide unindexed scans and deep pages.
SOAK_HEAVY = [
    ("fts_common", "/api/v1/events?q=error&limit=500"),
    ("history", "/api/v1/events?severity=error&since=24h&limit=500"),
    ("fts_common", "/api/v1/events?q=timeout&order=desc&limit=500"),
    ("history", "/api/v1/events?since=24h&order=desc&limit=500"),
]


class SoakQueries:
    """Every query is kept as (t, kind, ms, ok) so latency can be bucketed by hour."""

    def __init__(self, port, t0, interval=2.0, heavy_every=15):
        self.port, self.t0, self.interval, self.heavy_every = port, t0, interval, heavy_every
        self.log = []
        self._stop = threading.Event()
        self._t = threading.Thread(target=self._run, daemon=True)
        self._t.start()

    def _run(self):
        i = 0
        while not self._stop.wait(self.interval):
            kind, path = SOAK_QUERIES[i % len(SOAK_QUERIES)]
            if self.heavy_every and i % self.heavy_every == 0:
                kind, path = SOAK_HEAVY[(i // self.heavy_every) % len(SOAK_HEAVY)]
            i += 1
            ms, ok = timed_get(self.port, path, timeout=30)
            self.log.append((round(time.time() - self.t0, 1), kind, round(ms, 2), ok))

    def finish(self):
        self._stop.set()
        self._t.join()
        return self.log


def malloc_zone(pid):
    """macOS only: the default malloc zone row of `vmmap --summary` —
    allocated (live) bytes vs dirty bytes separates growth of live heap from
    allocator retention/fragmentation."""
    if sys.platform != "darwin":
        return None
    try:
        out = subprocess.run(["vmmap", "--summary", str(pid)], capture_output=True, text=True,
                             timeout=60).stdout
    except Exception:
        return None
    unit = {"K": 1 / 1024, "M": 1, "G": 1024}

    def mb(tok):
        try:
            return round(float(tok[:-1]) * unit[tok[-1]], 2)
        except (KeyError, ValueError, IndexError):
            return None
    for line in out.splitlines():
        if line.startswith("DefaultMallocZone"):
            f = line.split()
            # name, virtual, resident, dirty, swapped, count, allocated, frag, %frag, regions
            return {"zone_dirty_mb": mb(f[3]), "zone_allocations": int(f[5]),
                    "zone_allocated_mb": mb(f[6]), "zone_frag_mb": mb(f[7])}
    return None


def fts_stats(db):
    """Size of the FTS5 segment store vs live rows (read-only, short read txn)."""
    import sqlite3
    try:
        c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=10)
        rows, blob = c.execute("SELECT count(*), coalesce(sum(length(block)),0) FROM events_fts_data").fetchone()
        live = c.execute("SELECT count(*) FROM events").fetchone()[0]
        c.close()
        return {"fts_data_rows": rows, "fts_data_mb": round(blob / MB, 2), "live_events": live,
                "fts_bytes_per_live_event": round(blob / live, 1) if live else None}
    except Exception as e:  # the daemon may hold the write lock briefly
        return {"fts_error": str(e)}


def sc_soak(binary, extra, minutes, max_size_mb=32):
    """Long-run stability soak (M1.5-F). One 300 s cycle:
    60 s idle, 60 s @10/s, 60 s @100/s, 30 s @1000/s, 10 s @10000/s burst,
    80 s @10/s. Two SSE clients reconnect every cycle; a query every 2 s
    (every 15th a heavy history/FTS read); retention under `max_size_mb`
    pressure every 30 s. Samples memory, storage and retention every 10 s,
    and the malloc zone (macOS) + FTS segment size every 15 min."""
    cycle = [(0, 60), (10, 60), (100, 60), (1000, 30), (10000, 10), (10, 80)]
    period = sum(s for _, s in cycle)
    cycles = max(1, int(minutes * 60 // period))
    loadgen = os.path.join(EXAMPLES, "loadgen")
    lines, invocations = [], 0
    for _ in range(cycles):
        for rate, secs in cycle:
            if rate == 0:
                lines.append(f"sleep {secs}")
            else:
                lines.append(f"{loadgen} {rate} {secs} linger=0")
                invocations += 1
    expected = cycles * sum(r * s for r, s in cycle)
    keep = os.environ.get("TAILFLOW_BENCH_KEEP")
    with tempfile.TemporaryDirectory(dir=os.environ.get("TAILFLOW_BENCH_TMP")) as d:
        sh = os.path.join(d, "cycle.sh")
        with open(sh, "w") as f:
            f.write("#!/bin/sh\nsleep 5\n" + "\n".join(lines) + "\nsleep 86400\n")
        os.chmod(sh, 0o755)
        cfg = write_config(d, cmd=sh,
                           storage_extra=f"max_size_mb = {max_size_mb}\ncleanup_interval_secs = 30\n")
        p, port, _ = start(binary, extra, d, cfg)
        db = os.path.join(d, "events.db")
        t0 = time.time()
        first = procstat.snapshot(p.pid)
        ql = SoakQueries(port, t0)
        series, heap, sse, retention_passes = [], [], [], {}
        probes, next_probe, next_heap = [], t0 + 5, t0
        end = t0 + 5 + cycles * period
        alive = True
        while time.time() < end:
            now = time.time()
            if now >= next_probe:
                for pr in probes:
                    try:
                        sse.append({"t": round(now - t0), **probe_result(pr, 30)})
                    except Exception as e:
                        sse.append({"t": round(now - t0), "error": str(e)})
                probes = [probe(port, period - 2) for _ in range(2)]
                # Anchored to the start, so a 10 s sampling sleep cannot
                # make the schedule drift across an 8-hour run.
                while next_probe <= now:
                    next_probe += period
            if now >= next_heap:
                heap.append({"t": round(now - t0), **(malloc_zone(p.pid) or {}), **fts_stats(db)})
                next_heap = now + 900
            time.sleep(10)
            if p.poll() is not None:
                alive = False
                break
            snap = procstat.snapshot(p.pid)
            status = get(port, "/api/v1/storage/status") or {}
            h, dbs = status.get("health") or {}, status.get("db") or {}
            ret = (status.get("retention") or {})
            lc, at = ret.get("last_cleanup") or {}, ret.get("last_cleanup_at")
            if at and at not in retention_passes:
                retention_passes[at] = (round(time.time() - t0), lc.get("duration_ms"),
                                        lc.get("evicted_by_size"), lc.get("more_work"))
            ing = status.get("ingest") or {}
            if snap:
                series.append({
                    "t": round(time.time() - t0),
                    "rss_mb": round(snap["rss_bytes"] / MB, 2),
                    "private_mb": round((snap.get("private_bytes") or 0) / MB, 2),
                    "cpu_s": round(snap.get("cpu_s") or 0, 2),
                    "db_mb": round((dbs.get("file_bytes") or 0) / MB, 2),
                    "wal_mb": round((dbs.get("wal_bytes") or 0) / MB, 2),
                    "live_events": dbs.get("events"),
                    "persisted": h.get("events_persisted"),
                    "dropped": h.get("events_dropped_total"),
                    "queue": h.get("ingestion_queue_depth"),
                    "storage_errors": h.get("storage_errors"),
                    "ingest_dropped": ing.get("records_dropped"),
                    "sse_lag": ing.get("sse_client_lag_records"),
                })
        for pr in probes:
            try:
                sse.append({"t": round(time.time() - t0), **probe_result(pr, period)})
            except Exception as e:
                pr.kill()
                sse.append({"error": str(e)})
        heap.append({"t": round(time.time() - t0), **(malloc_zone(p.pid) or {}), **fts_stats(db)})
        # The generator runs hundreds of processes back to back and drifts
        # behind wall-clock time (≈1 s over 8 h). Stopping on schedule would
        # kill its last lines before they are written and look like loss, so
        # wait (bounded) until every line has reached the ring.
        want = expected + invocations
        drain_deadline = time.time() + 120
        while alive and time.time() < drain_deadline:
            cur = (get(port, "/health") or {}).get("cursor") or 0
            if cur >= want:
                break
            time.sleep(1)
        last = procstat.snapshot(p.pid)
        qlog = ql.finish()
        health = get(port, "/health") or {}
        st = storage_status(port)
        integrity = None
        if alive:
            res_shutdown = stop(p)
            import sqlite3
            try:
                c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
                integrity = c.execute("PRAGMA quick_check").fetchone()[0]
                c.close()
            except Exception as e:
                integrity = f"error: {e}"
        else:
            res_shutdown = None
        if keep:
            import shutil
            shutil.copy(db, keep)
        wall = time.time() - t0
        d_ = procstat.delta(first, last) or {}
        res = {
            "minutes": round(wall / 60, 1), "cycles": cycles, "max_size_mb": max_size_mb,
            "daemon_alive": alive,
            "generated": expected, "generator_invocations": invocations,
            "ingested_ring_cursor": health.get("cursor"),
            "ingest_dropped_reported": health.get("ingest_dropped"),
            "cpu_percent": round(100 * d_["cpu_s"] / wall, 3) if d_.get("cpu_s") is not None else None,
            "disk_write_mb": round(d_["disk_write_bytes"] / MB, 1) if d_.get("disk_write_bytes") is not None else None,
            "queries": len(qlog), "query_failed": sum(1 for q in qlog if not q[3]),
            "sse_gap_events": sum(x.get("gap_events") or 0 for x in sse),
            "sse_received": sum(x.get("received") or 0 for x in sse),
            "sse_probe_errors": sum(1 for x in sse if "error" in x),
            "integrity": integrity,
            **st,
            "shutdown_ms": res_shutdown,
            "series": series, "heap": heap, "sse": sse, "query_log": qlog,
            "retention_passes": sorted(retention_passes.values()),
        }
        return res


def run_scenario(spec, binary, extra):
    name, *args = spec.split(":")
    a = [int(x) for x in args]
    if name == "idle":
        return sc_idle(binary, extra, *(a or [60]))
    if name == "idle_file":
        return sc_idle(binary, extra, *(a or [60]), file_source=True)
    if name == "startup":
        return sc_startup(binary, extra)
    if name == "load":
        # load:<rate>[:seconds[:quiet_seconds]]
        return sc_load(binary, extra, a[0], a[1] if len(a) > 1 else 20,
                       quiet=a[2] if len(a) > 2 else 10)
    if name == "big":
        rate = a[0] if a else 100
        return sc_load(binary, extra, rate, a[3] if len(a) > 3 else 20,
                       big=a[1] if len(a) > 1 else 65536, big_every=a[2] if len(a) > 2 else 10)
    if name == "burst":
        return sc_burst(binary, extra, *(a or [10000]))
    if name == "fanout":
        return sc_load(binary, extra, 1000, 20, sse_clients=a[0] if a else 4)
    if name == "mixed":
        return sc_mixed(binary, extra, *(a or [60]))
    if name == "walamp":
        return sc_walamp(binary, extra, *a)
    if name == "querymem":
        return sc_querymem(binary, extra)
    if name == "endurance":
        return sc_endurance(binary, extra, *(a or [60]))
    if name == "soak":
        # soak:<minutes>[:max_size_mb]
        return sc_soak(binary, extra, *(a or [480]))
    raise SystemExit(f"unknown scenario {spec}")


SUMMARY_KEYS = [
    "cpu_percent", "wakeups_per_s", "disk_write_kb_per_s", "rss_mb_avg", "rss_mb_peak",
    "private_mb_avg", "private_mb_peak", "quiet_rss_mb_end", "quiet_private_mb_end",
    "bytes_written_per_event", "sse_p50_ms", "sse_p95_ms", "sse_p99_ms", "sse_lost_max", "sse_lost",
    "query_p50_ms", "query_p95_ms", "query_p99_ms", "shutdown_ms",
    "fresh_db_health_ms", "fresh_db_first_query_ms", "existing_db_health_ms",
    "existing_db_first_query_ms", "fresh_db_rss_mb", "db_bytes",
    "wal_bytes_per_event", "frames_per_event", "visible_p50_ms", "visible_p95_ms",
    "visible_p99_ms", "visible_max_ms",
    "rss_mb_before", "rss_mb_after", "rss_mb_after_30s", "private_mb_before",
    "private_mb_after", "private_mb_after_30s",
]


def summarize(runs):
    out = {}
    for k in SUMMARY_KEYS:
        vals = [r[k] for r in runs if isinstance(r.get(k), (int, float))]
        if not vals:
            continue
        med = statistics.median(vals)
        spread = (max(vals) - min(vals)) / med * 100 if med else 0.0
        out[k] = {"median": round(med, 3), "min": min(vals), "max": max(vals),
                  "spread_pct": round(spread, 1), "n": len(vals)}
    return out


def environment():
    env = {"platform": platform.platform(), "machine": platform.machine(),
           "python": platform.python_version()}
    try:
        if sys.platform == "darwin":
            env["cpu"] = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"],
                                        capture_output=True, text=True).stdout.strip()
            env["ram_gb"] = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True,
                                               text=True).stdout) // 1024 ** 3
        env["logical_cpus"] = os.cpu_count()
        env["load_avg"] = os.getloadavg()
    except Exception:
        pass
    return env


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", action="append", required=True, help="name=path[:extra args]")
    ap.add_argument("--scenarios", default="idle,startup,load:100,load:1000,load:10000")
    ap.add_argument("--repeat", type=int, default=1)
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
        print(line if len(line) < 2000 else line[:2000] + "…", flush=True)
        if out:
            out.write(line + "\n")
            out.flush()

    emit({"kind": "environment", **environment(), "bins": [b[0] for b in bins]})
    for spec in a.scenarios.split(","):
        runs = {name: [] for name, _, _ in bins}
        for i in range(a.repeat):
            for name, path, extra in bins:
                r = run_scenario(spec, path, extra)
                runs[name].append(r)
                emit({"kind": "run", "binary": name, "scenario": spec, "iteration": i, **r})
        for name in runs:
            emit({"kind": "summary", "binary": name, "scenario": spec, "stats": summarize(runs[name])})
    if out:
        out.close()


if __name__ == "__main__":
    main()
