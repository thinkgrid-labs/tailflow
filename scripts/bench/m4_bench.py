#!/usr/bin/env python3
"""M4 benchmarks: the cost of filesystem/Git observation, the agent stream
and authentication, against a pre-M4 baseline binary where that applies.

Scenarios (one JSON object per line):

  idle              no observation, silent source (baseline vs M4)
  idle_observe:<n>  M4 observing n project roots (2,000 files / 200 dirs
                    each, Git repositories), idle after startup
  monorepo          M4 observing one root of 20,000 directories / 60,000
                    files: time to "observing", memory, idle CPU
  burst             M4 observing one root while 10,000 files are written
                    in ~2 s: CPU, observations, coalescing, observer loss
  git_activity      M4 observing a repository while 20 files/s change for
                    30 s: snapshots taken, snapshot time, CPU
  log_load_stream   1,000 log lines/s for 20 s with the log SSE open, plus
                    (M4) the agent stream open (baseline vs M4)
  agents_stream     M4 with two simulated agents (hook calls at 10/s each)
                    and an agent-stream subscriber

Usage:
  python3 scripts/bench/m4_bench.py --base <old>/tailflow-daemon \\
      --m4 target/release/tailflow-daemon --tui target/release/tailflow \\
      [--only idle,burst] [--out results.jsonl]

CPU % is CPU time over wall time for the window (see resource_bench.py).
Every scenario starts a fresh daemon in a fresh temporary directory.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import resource_bench as rb  # noqa: E402


def git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                   env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"})


def make_tree(root, dirs, files_per_dir, repo=True):
    os.makedirs(root, exist_ok=True)
    for d in range(dirs):
        p = os.path.join(root, f"pkg{d // 50}", f"mod{d}")
        os.makedirs(p, exist_ok=True)
        for f in range(files_per_dir):
            with open(os.path.join(p, f"f{f}.rs"), "w") as fh:
                fh.write(f"// {d}/{f}\n")
    if repo:
        git(root, "init", "-q")
        git(root, "-c", "user.email=b@b", "-c", "user.name=b", "add", "-A")
        git(root, "-c", "user.email=b@b", "-c", "user.name=b", "commit", "-qm", "init")


class Projects:
    """A temporary directory for observed projects, outside the daemon's
    TAILFLOW_DATA_DIR (the daemon refuses to observe its own data)."""

    def __enter__(self):
        self._t = tempfile.TemporaryDirectory(prefix="tf-m4-proj-")
        return self._t.name

    def __exit__(self, *a):
        self._t.cleanup()


def observe_config(workdir, roots, extra=""):
    cfg = rb.write_config(workdir, cmd="sleep 86400")
    with open(cfg, "a") as f:
        paths = ", ".join(json.dumps(r) for r in roots)
        f.write(f"\n[observe]\npaths = [{paths}]\n{extra}")
    return cfg


def wait_observing(port, n, timeout=120):
    t = time.time()
    while time.time() - t < timeout:
        s = rb.get(port, "/api/v1/observe/status") or {}
        roots = s.get("roots") or []
        if len(roots) == n and all(r.get("state") == "observing" for r in roots):
            return round(time.time() - t, 2), s
        time.sleep(0.05)
    return None, rb.get(port, "/api/v1/observe/status")


def query_latency(port):
    """Median of 5 calls to the M4 evidence endpoints, in ms."""
    out = {}
    for name, path in (("evidence_files_ms", "/api/v1/evidence/files?limit=200"),
                       ("outcomes_ms", "/api/v1/outcomes"),
                       ("observe_status_ms", "/api/v1/observe/status")):
        ms = sorted(rb.timed_get(port, path)[0] for _ in range(5))
        out[name] = round(ms[2], 1)
    return out


def sc_idle(binary, seconds):
    return rb.sc_idle(binary, [], seconds=seconds)


def sc_idle_observe(binary, n, seconds):
    with tempfile.TemporaryDirectory() as d, Projects() as pd:
        roots = []
        for i in range(n):
            r = os.path.join(pd, f"proj{i}")
            make_tree(r, 200, 10)
            roots.append(r)
        cfg = observe_config(d, roots)
        p, port, _ = rb.start(binary, [], d, cfg)
        ready_s, st = wait_observing(port, n)
        time.sleep(5)
        s = rb.Sampler(p.pid, every=1.0)
        time.sleep(seconds)
        res = s.finish()
        st = rb.get(port, "/api/v1/observe/status") or {}
        res.update({
            "roots": n, "observing_after_s": ready_s,
            "watched_dirs": sum(r.get("watched_dirs", 0) for r in st.get("roots", [])),
            "git_snapshots": sum(r.get("snapshots", 0) for r in st.get("repos", [])),
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def sc_monorepo(binary, seconds):
    with tempfile.TemporaryDirectory() as d, Projects() as pd:
        r = os.path.join(pd, "mono")
        t = time.time()
        make_tree(r, 20_000, 3, repo=True)
        build_s = round(time.time() - t, 1)
        cfg = observe_config(d, [r])
        p, port, _ = rb.start(binary, [], d, cfg)
        ready_s, _ = wait_observing(port, 1, timeout=300)
        time.sleep(5)
        s = rb.Sampler(p.pid, every=1.0)
        time.sleep(seconds)
        res = s.finish()
        st = rb.get(port, "/api/v1/observe/status") or {}
        root = (st.get("roots") or [{}])[0]
        repo = (st.get("repos") or [{}])[0]
        res.update({
            "dirs": 20_000, "files": 60_000, "tree_build_s": build_s, "observing_after_s": ready_s,
            "watched_dirs": root.get("watched_dirs"), "unwatched_dirs": root.get("unwatched_dirs"),
            "git_snapshot_ms": repo.get("last_took_ms"), "git_snapshots": repo.get("snapshots"),
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def sc_burst(binary):
    with tempfile.TemporaryDirectory() as d, Projects() as pd:
        r = os.path.join(pd, "proj")
        make_tree(r, 50, 2)
        cfg = observe_config(d, [r], "git_min_interval_secs = 5\n")
        p, port, _ = rb.start(binary, [], d, cfg)
        wait_observing(port, 1)
        time.sleep(2)
        s = rb.Sampler(p.pid, every=0.25)
        t = time.time()
        out = os.path.join(r, "gen")
        os.makedirs(out, exist_ok=True)
        for i in range(10_000):
            with open(os.path.join(out, f"g{i}.txt"), "w") as fh:
                fh.write("x")
        write_s = round(time.time() - t, 2)
        time.sleep(8)  # coalescing, flush, a Git snapshot
        res = s.finish()
        st = rb.get(port, "/api/v1/observe/status") or {}
        root = (st.get("roots") or [{}])[0]
        repo = (st.get("repos") or [{}])[0]
        res.update({
            "files_written": 10_000, "write_s": write_s,
            "observations": root.get("observations"), "overflowed": root.get("overflowed"),
            "rescans": root.get("rescans"), "pending_paths_end": root.get("pending_paths"),
            "git_snapshots": repo.get("snapshots"), "git_snapshot_ms": repo.get("last_took_ms"),
            **rb.storage_status(port), **query_latency(port),
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def sc_git_activity(binary, seconds=30, rate=20):
    with tempfile.TemporaryDirectory() as d, Projects() as pd:
        r = os.path.join(pd, "proj")
        make_tree(r, 200, 10)
        cfg = observe_config(d, [r], "git_min_interval_secs = 5\n")
        p, port, _ = rb.start(binary, [], d, cfg)
        wait_observing(port, 1)
        time.sleep(3)
        s = rb.Sampler(p.pid, every=0.5)
        t = time.time()
        n = 0
        while time.time() - t < seconds:
            dd = random.randrange(200)
            path = os.path.join(r, f"pkg{dd // 50}", f"mod{dd}", f"f{random.randrange(10)}.rs")
            with open(path, "a") as fh:
                fh.write(f"// edit {n}\n")
            n += 1
            time.sleep(1 / rate)
        time.sleep(8)
        res = s.finish()
        st = rb.get(port, "/api/v1/observe/status") or {}
        repo = (st.get("repos") or [{}])[0]
        root = (st.get("roots") or [{}])[0]
        res.update({
            "edits": n, "seconds": seconds, "observations": root.get("observations"),
            "git_snapshots": repo.get("snapshots"), "git_snapshot_ms_last": repo.get("last_took_ms"),
            "git_error": repo.get("last_error"), **query_latency(port),
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def agent_stream_reader(port, seconds, out):
    """Hold /api/v1/stream/agent open; count `agent` events."""
    req = urllib.request.Request(f"http://127.0.0.1:{port}/api/v1/stream/agent", headers=rb.AUTH)
    n = 0
    t = time.time()
    try:
        with urllib.request.urlopen(req, timeout=seconds + 5) as r:
            while time.time() - t < seconds:
                line = r.readline()
                if not line:
                    break
                if line.startswith(b"event: agent"):
                    n += 1
    except Exception as e:  # noqa: BLE001
        out["error"] = str(e)[:120]
    out["agent_events"] = n


def sc_log_load_stream(binary, with_agent_stream, rate=1000, seconds=20):
    with tempfile.TemporaryDirectory() as d:
        cfg = rb.write_config(d, cmd=rb.gen_cmd(rate, seconds))
        p, port, _ = rb.start(binary, [], d, cfg)
        pr = rb.probe(port, seconds + 6)
        stream = {}
        th = None
        if with_agent_stream:
            th = threading.Thread(target=agent_stream_reader, args=(port, seconds + 4, stream), daemon=True)
            th.start()
        time.sleep(2)
        s = rb.Sampler(p.pid)
        time.sleep(seconds + 1)
        res = s.finish()
        sse = rb.probe_result(pr, seconds + 40)
        if th:
            th.join(timeout=10)
        res.update({
            "rate": rate, "agent_stream_open": with_agent_stream,
            "sse_received": sse.get("received"), "sse_p95_ms": sse.get("p95_ms"),
            "agent_stream": stream or None, **rb.storage_status(port),
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def hook(tui, data_dir, payload):
    t = time.perf_counter()
    subprocess.run([tui, "agent", "hook", "claude"], input=json.dumps(payload).encode(),
                   env={**os.environ, "TAILFLOW_DATA_DIR": data_dir},
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
    return (time.perf_counter() - t) * 1000


def sc_agents_stream(binary, tui, seconds=30, rate=10):
    with tempfile.TemporaryDirectory() as d:
        cfg = rb.write_config(d, cmd="sleep 86400")
        p, port, _ = rb.start(binary, ["--agents"], d, cfg)
        stream = {}
        th = threading.Thread(target=agent_stream_reader, args=(port, seconds + 8, stream), daemon=True)
        th.start()
        time.sleep(2)
        s = rb.Sampler(p.pid, every=0.5)
        lat = []
        sent = [0]
        lock = threading.Lock()

        def agent(k):
            sid = f"bench-{k}"
            t = time.time()
            i = 0
            while time.time() - t < seconds:
                payload = {"hook_event_name": "PostToolUse", "session_id": sid, "cwd": d, "tool_name": "Bash",
                           "tool_use_id": f"{sid}-{i}", "tool_input": {"command": "cargo test"}, "tool_response": {}}
                ms = hook(tui, d, payload)
                with lock:
                    lat.append(ms)
                    sent[0] += 1
                i += 1
                time.sleep(max(0, 1 / rate - ms / 1000))

        ws = [threading.Thread(target=agent, args=(k,)) for k in range(2)]
        for w in ws:
            w.start()
        for w in ws:
            w.join()
        time.sleep(6)
        res = s.finish()
        th.join(timeout=15)
        lat.sort()
        res.update({
            "agents": 2, "hook_calls": sent[0],
            "hook_ms_p50": round(lat[len(lat) // 2], 1) if lat else None,
            "hook_ms_p95": round(lat[int(len(lat) * 0.95)], 1) if lat else None,
            "agent_stream": stream,
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--m4", required=True)
    ap.add_argument("--tui", required=True, help="M4 `tailflow` binary (agent hooks)")
    ap.add_argument("--only", default="")
    ap.add_argument("--idle-seconds", type=int, default=60)
    ap.add_argument("--out", default="-")
    a = ap.parse_args()
    only = set(filter(None, a.only.split(",")))
    out = sys.stdout if a.out == "-" else open(a.out, "a")

    def emit(name, binary_name, fn):
        if only and name.split(":")[0] not in only and name not in only:
            return
        t = time.time()
        try:
            r = fn()
        except Exception as e:  # noqa: BLE001
            r = {"error": str(e)[:300]}
        r = {"scenario": name, "binary": binary_name, "load_avg": os.getloadavg()[0],
             "took_s": round(time.time() - t, 1), **r}
        out.write(json.dumps(r) + "\n")
        out.flush()

    for name, b in (("base", a.base), ("m4", a.m4)):
        emit("idle", name, lambda b=b: sc_idle(b, a.idle_seconds))
    for n in (1, 4):
        emit(f"idle_observe:{n}", "m4", lambda n=n: sc_idle_observe(a.m4, n, a.idle_seconds))
    emit("monorepo", "m4", lambda: sc_monorepo(a.m4, 30))
    emit("burst", "m4", lambda: sc_burst(a.m4))
    emit("git_activity", "m4", lambda: sc_git_activity(a.m4))
    emit("log_load_stream", "base", lambda: sc_log_load_stream(a.base, False))
    emit("log_load_stream", "m4", lambda: sc_log_load_stream(a.m4, True))
    emit("agents_stream", "m4", lambda: sc_agents_stream(a.m4, a.tui))


if __name__ == "__main__":
    main()
