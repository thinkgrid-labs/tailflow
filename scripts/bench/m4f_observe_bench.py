#!/usr/bin/env python3
"""M4-F observer benchmarks: idle wakeups and burst reliability of
filesystem observation, comparing two or more daemon builds interleaved
(each repeat runs every binary once, in rotating order, so host drift
affects all alike).

Scenarios (one JSON object per line):

  idle_observe:<n>    n observed roots (2,000 files / 200 dirs each, Git
                      repositories), idle; n = 0 is observation off
  idle_dashboard      1 observed root, idle, with the log SSE (/events)
                      and the agent stream held open, as an open
                      dashboard tab does
  monorepo            1 root of 20,000 dirs / 60,000 files, idle (the tree
                      is built once and shared by every binary)
  burst:<rate>:<n>:<mode>
                      n file writes paced at <rate>/s under one observed
                      root. mode: unique (n new files), same (n appends to
                      one file), mix (4 threads of create, rename, delete)

Recorded per run: CPU %, wakeups/s, RSS/private memory, bytes written,
and from /api/v1/observe/status the observer's counters: observations,
overflowed (dropped and recorded as loss), rescans, and, from M4-F, raw
OS notifications, notifications coalesced in the callback and the queue
high-water mark. `settle_s` is the time from the last write until the
observation count stops changing (an upper bound on visibility delay).

Raw notifications and observations are not one-to-one: the OS may send
several notifications per write (or merge several writes), and the
observer reports each path's net change once per quiet period.

Usage:
  python3 scripts/bench/m4f_observe_bench.py \\
      --bin m4=<m4 daemon> --bin m4f=<m4-f daemon> \\
      [--only idle_observe,burst] [--repeat 3] [--idle-seconds 60] \\
      [--bursts 100:3000:unique,1000:10000:unique] [--out results.jsonl]
"""

import argparse
import json
import os
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import resource_bench as rb  # noqa: E402
import m4_bench as m4  # noqa: E402

DEFAULT_BURSTS = ",".join([
    "100:3000:unique", "500:5000:unique", "1000:10000:unique", "2500:10000:unique",
    "5000:10000:unique", "100000:10000:unique",  # 100000/s = as fast as possible
    "2000:10000:same", "1000:4000:mix",
])


def root_counts(port):
    st = rb.get(port, "/api/v1/observe/status") or {}
    roots = st.get("roots") or []
    keys = ("observations", "overflowed", "rescans", "excluded", "pending_paths",
            "notifications", "coalesced", "queue_peak", "watched_dirs", "unwatched_dirs")
    out = {k: sum((r.get(k) or 0) for r in roots) if any(k in r for r in roots) else None for k in keys}
    out["git_snapshots"] = sum(r.get("snapshots", 0) for r in st.get("repos", []))
    return out


def idle_observe(binary, n, seconds, trees):
    with tempfile.TemporaryDirectory() as d:
        if n == 0:
            cfg = rb.write_config(d, cmd="sleep 86400")
        else:
            cfg = m4.observe_config(d, trees[:n])
        p, port, _ = rb.start(binary, [], d, cfg)
        ready_s = None
        if n:
            ready_s, _ = m4.wait_observing(port, n, timeout=300)
        time.sleep(5)
        s = rb.Sampler(p.pid, every=1.0)
        time.sleep(seconds)
        res = s.finish()
        res.update({"roots": n, "observing_after_s": ready_s, **root_counts(port)})
        res["shutdown_ms"] = rb.stop(p)
        return res


def hold_stream(port, path, stop, out, key):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=rb.AUTH)
    n = 0
    try:
        with urllib.request.urlopen(req, timeout=3600) as r:
            while not stop.is_set():
                line = r.readline()
                if not line:
                    break
                n += 1
    except Exception as e:  # noqa: BLE001
        out[key + "_error"] = str(e)[:120]
    out[key + "_lines"] = n


def idle_dashboard(binary, seconds, trees):
    with tempfile.TemporaryDirectory() as d:
        cfg = m4.observe_config(d, trees[:1])
        p, port, _ = rb.start(binary, ["--agents"], d, cfg)
        m4.wait_observing(port, 1)
        stop = threading.Event()
        streams = {}
        ths = [threading.Thread(target=hold_stream, args=(port, path, stop, streams, key), daemon=True)
               for path, key in (("/events", "log_sse"), ("/api/v1/stream/agent", "agent_sse"))]
        for t in ths:
            t.start()
        time.sleep(5)
        s = rb.Sampler(p.pid, every=1.0)
        time.sleep(seconds)
        res = s.finish()
        res.update({"roots": 1, "streams_open": 2, **root_counts(port)})
        # SIGTERM with both streams still open (they must end on shutdown).
        res["shutdown_ms"] = rb.stop(p)
        stop.set()
        for t in ths:
            t.join(timeout=5)
        res.update(streams)
        return res


def paced(n, rate, step):
    """Call step(i) n times at `rate`/s; returns the achieved rate."""
    t0 = time.perf_counter()
    for i in range(n):
        target = t0 + i / rate
        now = time.perf_counter()
        if target > now:
            time.sleep(target - now)
        step(i)
    return n / max(time.perf_counter() - t0, 1e-9)


def burst(binary, rate, n, mode, projects):
    with tempfile.TemporaryDirectory() as d, m4.Projects() as pd:
        r = os.path.join(pd, "proj")
        m4.make_tree(r, 50, 2)
        cfg = m4.observe_config(d, [r], "git_min_interval_secs = 5\n")
        p, port, _ = rb.start(binary, [], d, cfg)
        m4.wait_observing(port, 1)
        time.sleep(2)
        before = root_counts(port)
        out = os.path.join(r, "gen")
        os.makedirs(out, exist_ok=True)
        s = rb.Sampler(p.pid, every=0.25)
        if mode == "unique":
            def step(i):
                with open(os.path.join(out, f"g{i}.txt"), "w") as fh:
                    fh.write("x")
            achieved = paced(n, rate, step)
            expected = n
        elif mode == "same":
            target = os.path.join(out, "same.log")

            def step(i):
                with open(target, "a") as fh:
                    fh.write(f"{i}\n")
            achieved = paced(n, rate, step)
            expected = None  # coalesced: a few observations of one path
        else:  # mix: create, rename, delete in 4 threads
            per = n // 4
            rates = []

            def worker(k):
                def step(i):
                    a = os.path.join(out, f"m{k}-{i}.tmpx")
                    b = os.path.join(out, f"m{k}-{i}.txt")
                    with open(a, "w") as fh:
                        fh.write("x")
                    os.rename(a, b)
                    if i % 2:
                        os.remove(b)
                rates.append(paced(per, rate / 4, step))
            ts = [threading.Thread(target=worker, args=(k,)) for k in range(4)]
            for t in ts:
                t.start()
            for t in ts:
                t.join()
            achieved = sum(rates)
            expected = None
        t_end = time.time()
        # Wait until observations stop changing (coalescing, max hold).
        last, stable_since = None, time.time()
        while time.time() - t_end < 60:
            c = root_counts(port)
            if c["observations"] != last:
                last, stable_since = c["observations"], time.time()
            elif time.time() - stable_since >= 3:
                break
            time.sleep(0.1)
        settle_s = round(stable_since - t_end, 2)
        res = s.finish()
        after = root_counts(port)
        delta = {k: (after[k] - before[k]) if isinstance(after.get(k), int) and isinstance(before.get(k), int)
                 else after.get(k) for k in after}
        res.update({
            "rate_target": rate, "files": n, "mode": mode, "rate_achieved": round(achieved, 1),
            "expected_observations": expected, "settle_s": settle_s, **delta,
            **rb.storage_status(port),
        })
        res["shutdown_ms"] = rb.stop(p)
        return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", action="append", required=True, help="name=path (repeatable)")
    ap.add_argument("--only", default="")
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--idle-seconds", type=int, default=60)
    ap.add_argument("--bursts", default=DEFAULT_BURSTS)
    ap.add_argument("--out", default="-")
    a = ap.parse_args()
    bins = [tuple(b.split("=", 1)) for b in a.bin]
    only = set(filter(None, a.only.split(",")))
    out = sys.stdout if a.out == "-" else open(a.out, "a")

    def want(name):
        return not only or name in only or name.split(":")[0] in only

    def emit(name, bname, rep, fn):
        t = time.time()
        try:
            r = fn()
        except Exception as e:  # noqa: BLE001
            r = {"error": str(e)[:300]}
        r = {"scenario": name, "binary": bname, "repeat": rep, "load_avg": round(os.getloadavg()[0], 1),
             "took_s": round(time.time() - t, 1), **r}
        out.write(json.dumps(r) + "\n")
        out.flush()

    with m4.Projects() as pd:
        trees = []
        if want("idle_observe") or want("idle_dashboard"):
            for i in range(4):
                t = os.path.join(pd, f"proj{i}")
                m4.make_tree(t, 200, 10)
                trees.append(t)
        mono = None
        if want("monorepo"):
            mono = os.path.join(pd, "mono")
            m4.make_tree(mono, 20_000, 3)
        for rep in range(a.repeat):
            order = bins[rep % len(bins):] + bins[:rep % len(bins)]
            for n in (0, 1, 4):
                if want(f"idle_observe:{n}"):
                    for name, b in order:
                        emit(f"idle_observe:{n}", name, rep, lambda b=b, n=n: idle_observe(b, n, a.idle_seconds, trees))
            if want("idle_dashboard"):
                for name, b in order:
                    emit("idle_dashboard", name, rep, lambda b=b: idle_dashboard(b, a.idle_seconds, trees))
            if mono:
                for name, b in order:
                    emit("monorepo", name, rep, lambda b=b: idle_observe(b, 1, a.idle_seconds, [mono]))
            if want("burst"):
                for spec in filter(None, a.bursts.split(",")):
                    rate, n, mode = spec.split(":")
                    for name, b in order:
                        emit(f"burst:{spec}", name, rep,
                             lambda b=b, rate=rate, n=n, mode=mode: burst(b, float(rate), int(n), mode, pd))


if __name__ == "__main__":
    main()
