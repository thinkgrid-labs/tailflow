#!/usr/bin/env python3
"""M2 agent-capture benchmarks. Everything runs in a temporary
TAILFLOW_DATA_DIR; nothing touches real agent configs or the user's spool.

Scenarios (prints one JSON object per scenario):

  hook          per-call wall time of `tailflow agent hook claude` for real
                fixture payloads (the latency a sync hook adds to the agent),
                against `tailflow --version` (same binary, no hook work) and
                /usr/bin/true (process spawn floor)
  hook_full     the same with 20,000 records already waiting (spool full:
                the overflow path, and the worst directory scan)
  hook_big      PostToolUse for a Write of a 1 MiB file
  agents:<n>    a daemon with --agents and n concurrent simulated agents,
                each making hook calls at --rate per second for --seconds:
                daemon CPU / memory, hook latency under load, hook-to-
                queryable lag, storage bytes and full-text bytes per event,
                and loss counters
  idle_agents   a daemon with --agents and no log sources, idle

Usage:
  agent_bench.py --bin target/release [--only hook,agents:1,agents:4] [--runs 3]
"""

import argparse
import json
import os
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(__file__))
import procstat  # noqa: E402

FIXTURES = os.path.join(os.path.dirname(__file__), "../../crates/tailflow-core/tests/fixtures/claude-code-2.1.331")
MB = 1024 * 1024


def pct(v, p):
    if not v:
        return None
    s = sorted(v)
    return round(s[min(len(s) - 1, int(round(p / 100 * (len(s) - 1))))], 3)


def stats(v):
    return {"p50_ms": pct(v, 50), "p95_ms": pct(v, 95), "p99_ms": pct(v, 99),
            "max_ms": round(max(v), 3) if v else None, "n": len(v)}


def fixtures():
    out = []
    for name in sorted(os.listdir(FIXTURES)):
        if name.endswith(".json"):
            with open(os.path.join(FIXTURES, name)) as f:
                out.append(json.load(f))
    return out


def timed(cmd, data, env):
    t = time.perf_counter()
    r = subprocess.run(cmd, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    ms = (time.perf_counter() - t) * 1000
    assert r.returncode == 0, (cmd, r.returncode, r.stderr[:200])
    if "hook" in cmd:
        assert r.stdout == b"", "the hook must never write to stdout"
    return ms


def sc_hook(binary, n=300, prefill=0, big=False):
    tf = os.path.join(binary, "tailflow")
    with tempfile.TemporaryDirectory() as d:
        env = {**os.environ, "TAILFLOW_DATA_DIR": d}
        spool = os.path.join(d, "agents", "spool")
        os.makedirs(spool, exist_ok=True)
        for i in range(prefill):
            with open(os.path.join(spool, f"{i:012x}-claude-code-prefill{i}.json"), "w") as f:
                f.write("{}")
        payloads = fixtures()
        if big:
            p = next(x for x in payloads if x["hook_event_name"] == "PostToolUse")
            p = dict(p, tool_name="Write", tool_input={"file_path": "/work/proj/big.txt",
                                                        "content": "x" * MB},
                     tool_response={"type": "create"})
            payloads = [p]
        datas = [json.dumps(p).encode() for p in payloads]
        for _ in range(5):  # warm the page cache / first-launch assessment
            timed([tf, "agent", "hook", "claude"], datas[0], env)
        hook = [timed([tf, "agent", "hook", "claude"], datas[i % len(datas)], env) for i in range(n)]
        version = [timed([tf, "--version"], b"", env) for _ in range(n)] if not prefill and not big else []
        true = [timed(["/usr/bin/true"], b"", env) for _ in range(n)] if not prefill and not big else []
        written = len([x for x in os.listdir(spool) if x.endswith(".json") and x != "overflow.json"]) - prefill
        overflow = None
        if os.path.exists(os.path.join(spool, "overflow.json")):
            with open(os.path.join(spool, "overflow.json")) as f:
                overflow = json.load(f).get("count")
        res = {"hook": stats(hook), "records_written": written, "overflow_count": overflow}
        if version:
            res["tailflow_version"] = stats(version)
            res["bin_true"] = stats(true)
            res["hook_minus_version_p50_ms"] = round(pct(hook, 50) - pct(version, 50), 3)
        return res


def get(port, path, timeout=5):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=timeout) as r:
            return json.loads(r.read())
    except Exception:
        return None


def free_port():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def start_daemon(binary, d, extra=()):
    port = free_port()
    cfg = os.path.join(d, "tailflow.toml")
    with open(cfg, "w") as f:
        f.write(f'[storage]\npath = "{os.path.join(d, "events.db")}"\n')
    p = subprocess.Popen([os.path.join(binary, "tailflow-daemon"), "--config", cfg, "--port", str(port),
                          "--agents", *extra],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         env={**os.environ, "RUST_LOG": "error", "TAILFLOW_DATA_DIR": d}, cwd=d)
    deadline = time.time() + 30
    while time.time() < deadline:
        if get(port, "/health", timeout=1):
            return p, port
        time.sleep(0.02)
    p.kill()
    raise RuntimeError("daemon did not start")


def stop(p):
    t = time.perf_counter()
    p.terminate()
    try:
        p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        p.kill()
        return None
    return round((time.perf_counter() - t) * 1000, 1)


class Sampler:
    def __init__(self, pid, every=0.5):
        self.pid, self.every, self.rss, self.private = pid, every, [], []
        self.first, self.t0 = procstat.snapshot(pid), time.time()
        self._stop = threading.Event()
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
        d = procstat.delta(self.first, procstat.snapshot(self.pid)) or {}
        wall = time.time() - self.t0
        r = lambda v, n=2: None if v is None else round(v, n)  # noqa: E731
        return {"cpu_percent": r(100 * d["cpu_s"] / wall, 3) if d.get("cpu_s") is not None else None,
                "wakeups_per_s": r(d["wakeups"] / wall, 2) if d.get("wakeups") is not None else None,
                "rss_mb_peak": r(max(self.rss) / MB, 1) if self.rss else None,
                "private_mb_avg": r(statistics.mean(self.private) / MB, 1) if self.private else None,
                "private_mb_peak": r(max(self.private) / MB, 1) if self.private else None}


def sc_idle_agents(binary, seconds=60):
    with tempfile.TemporaryDirectory() as d:
        p, port = start_daemon(binary, d)
        time.sleep(5)
        s = Sampler(p.pid, every=1.0)
        time.sleep(seconds)
        res = s.finish()
        res["shutdown_ms"] = stop(p)
        return res


def sc_agents(binary, n, rate, seconds):
    tf = os.path.join(binary, "tailflow")
    payloads = fixtures()
    with tempfile.TemporaryDirectory() as d:
        env = {**os.environ, "TAILFLOW_DATA_DIR": d}
        p, port = start_daemon(binary, d)
        time.sleep(2)
        lat, lock = [], threading.Lock()
        calls = [0] * n

        def agent(k):
            sid = f"bench{k:04d}-0000-4000-8000-000000000000"
            interval = 1.0 / rate
            nxt = time.perf_counter()
            end = nxt + seconds
            i = 0
            while time.perf_counter() < end:
                v = dict(payloads[i % len(payloads)])
                v["session_id"] = sid
                if "tool_use_id" in v:
                    v["tool_use_id"] = f"{v['tool_use_id']}-{k}-{i // len(payloads)}"
                if "prompt_id" in v:
                    v["prompt_id"] = f"p{k}-{i // len(payloads)}"
                ms = timed([tf, "agent", "hook", "claude"], json.dumps(v).encode(), env)
                with lock:
                    lat.append(ms)
                calls[k] += 1
                i += 1
                nxt += interval
                time.sleep(max(0.0, nxt - time.perf_counter()))

        s = Sampler(p.pid)
        threads = [threading.Thread(target=agent, args=(k,)) for k in range(n)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        # Lag: one more call, then poll until it is queryable.
        probe_sid = "probe000-0000-4000-8000-000000000000"
        t0 = time.perf_counter()
        timed([tf, "agent", "hook", "claude"],
              json.dumps({"hook_event_name": "SessionStart", "session_id": probe_sid,
                          "cwd": d, "source": "startup"}).encode(), env)
        lag = None
        while time.perf_counter() - t0 < 60:
            page = get(port, f"/api/v1/sessions/claude-code:{probe_sid}")
            if page and page.get("session_id"):
                lag = round((time.perf_counter() - t0) * 1000, 1)
                break
            time.sleep(0.01)
        time.sleep(1)
        res = s.finish()
        st = get(port, "/api/v1/storage/status") or {}
        ag = get(port, "/api/v1/agents/status") or {}
        res.update({
            "agents": n, "rate_per_agent": rate, "hook_calls": sum(calls),
            "hook": stats(lat), "hook_to_queryable_ms": lag,
            "persisted": st.get("health", {}).get("events_persisted"),
            "dropped_total": st.get("health", {}).get("events_dropped_total"),
            "max_queue_depth_seen": st.get("health", {}).get("ingestion_queue_depth"),
            "files_ingested": ag.get("ingest", {}).get("files_ingested"),
            "spool_pending_after": (ag.get("spool") or {}).get("pending_files"),
        })
        res["shutdown_ms"] = stop(p)
        db = os.path.join(d, "events.db")
        c = sqlite3.connect(db)
        events = c.execute("SELECT count(*) FROM events WHERE source_type = 'agent_hook'").fetchone()[0]
        size = sum(os.path.getsize(db + x) for x in ("", "-wal") if os.path.exists(db + x))
        try:
            fts = c.execute("SELECT coalesce(sum(length(block)), 0) FROM events_fts_data").fetchone()[0]
        except sqlite3.Error:
            fts = None
        sessions = c.execute("SELECT count(*), sum(event_count) FROM agent_sessions").fetchone()
        c.close()
        res.update({"agent_events": events, "db_bytes_per_event": round(size / events, 1) if events else None,
                    "fts_bytes_per_event": round(fts / events, 1) if events and fts else None,
                    "sessions": sessions[0], "session_event_sum": sessions[1]})
        return res


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True, help="directory with tailflow and tailflow-daemon")
    ap.add_argument("--only", default="hook,hook_full,hook_big,idle_agents,agents:1,agents:2,agents:4")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--rate", type=float, default=10.0, help="hook calls per second per agent")
    ap.add_argument("--seconds", type=int, default=30)
    a = ap.parse_args()
    binary = os.path.abspath(a.bin)
    for spec in a.only.split(","):
        name, *arg = spec.split(":")
        for run in range(a.runs):
            if name == "hook":
                r = sc_hook(binary)
            elif name == "hook_full":
                r = sc_hook(binary, n=100, prefill=20_000)
            elif name == "hook_big":
                r = sc_hook(binary, n=100, big=True)
            elif name == "idle_agents":
                r = sc_idle_agents(binary, int(arg[0]) if arg else 60)
            elif name == "agents":
                r = sc_agents(binary, int(arg[0]), a.rate, a.seconds)
            else:
                raise SystemExit(f"unknown scenario {spec}")
            print(json.dumps({"scenario": spec, "run": run, **r}), flush=True)


if __name__ == "__main__":
    main()
