#!/usr/bin/env python3
"""Deterministic resource checks for CI (shared runners).

  python3 scripts/bench/check_budgets.py [--target-dir target] [--out results.jsonl]

Shared runners make timing noise, so nothing here gates on CPU or latency.
It gates only on properties that do not depend on runner speed:

  * zero event loss under short steady, burst, and large-payload loads
    (SSE delivery, ingestion bus, and storage), and every generated line
    persisted;
  * release binary sizes within `budgets.json` (+ its headroom);
  * memory bounded: peak and post-load resident/private memory under
    generous ceilings that only a leak or an unbounded buffer would exceed.

Timing results are still written to --out for the record.
"""

import argparse
import json
import os
import platform
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import resource_bench as rb  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target-dir", default=os.path.join(rb.ROOT, "target"))
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    release = os.path.join(a.target_dir, "release")
    rb.EXAMPLES = os.path.join(release, "examples")
    budgets = json.load(open(os.path.join(HERE, "budgets.json")))
    failures = []
    records = []

    # ── Binary size ──
    key = f"{platform.system().lower()}-{platform.machine().lower()}"
    sizes = budgets["binary_bytes"].get(key)
    headroom = 1 + budgets["binary_headroom_pct"] / 100
    for name in ("tailflow", "tailflow-daemon", "tailflow-mcp", "tailflow-logs"):
        path = os.path.join(release, name + (".exe" if os.name == "nt" else ""))
        if not os.path.exists(path):
            failures.append(f"missing release binary {path}")
            continue
        size = os.path.getsize(path)
        records.append({"check": "binary_size", "binary": name, "bytes": size})
        if sizes and name in sizes and size > sizes[name] * headroom:
            failures.append(f"{name}: {size} bytes exceeds budget {sizes[name]} (+{budgets['binary_headroom_pct']}%)")
    if not sizes:
        print(f"note: no size budget recorded for {key}; sizes reported only", file=sys.stderr)

    # ── Loss and memory ──
    daemon = os.path.join(release, "tailflow-daemon")
    mem = budgets["memory_mb"]
    runs = [
        ("idle", lambda: rb.sc_idle(daemon, [], 15)),
        ("load_1000", lambda: rb.sc_load(daemon, [], 1000, 10)),
        ("burst_5000", lambda: rb.sc_burst(daemon, [], 5000, 10)),
        ("big_50x64k", lambda: rb.sc_load(daemon, [], 50, 10, big=65536)),
    ]
    for name, run in runs:
        r = run()
        records.append({"check": name, **r})

        def need(key, cond, msg):
            # A missing reading is a failure: a crashed daemon or probe must
            # never look like "nothing was lost".
            v = r.get(key)
            if v is None:
                failures.append(f"{name}: no value for {key} (daemon or probe failed?)")
            elif not cond(v):
                failures.append(f"{name}: {msg} ({key}={v})")

        need("daemon_alive", bool, "daemon exited during the run")
        need("shutdown_ms", lambda v: True, "")
        if name == "idle":
            need("rss_mb_peak", lambda v: v <= mem["idle_peak"], f"peak RSS over {mem['idle_peak']} MB")
            continue
        gen = r.get("generated")
        need("sse_received_min", lambda v: v > 0, "an SSE client received nothing")
        need("sse_lost_max" if "sse_lost_max" in r else "sse_lost", lambda v: v == 0, "SSE client lost records")
        need("ingest_dropped_reported", lambda v: v == 0, "ingestion dropped records")
        need("storage_dropped", lambda v: v == 0, "storage dropped events")
        # The generator's only extra output is its one stderr line
        # ("loadgen: N lines"), so the ring must hold exactly gen + 1.
        need("ingested_ring_cursor", lambda v: v == gen + 1, f"ring cursor != generated + 1 ({gen + 1})")
        cursor = r.get("ingested_ring_cursor") or 0
        need("persisted", lambda v: v >= cursor, "fewer events persisted than ingested")
        need("rss_mb_peak", lambda v: v <= mem["load_peak"], f"peak RSS over {mem['load_peak']} MB")
        if "quiet_rss_mb_end" in r:
            need("quiet_rss_mb_end", lambda v: v <= mem["after_load"], f"RSS after load over {mem['after_load']} MB")

    if a.out:
        with open(a.out, "w") as f:
            for r in records:
                f.write(json.dumps(r) + "\n")
    for r in records:
        print(json.dumps({k: v for k, v in r.items() if k != "series"}))
    if failures:
        print("\nresource checks FAILED:", file=sys.stderr)
        for f in failures:
            print("  " + f, file=sys.stderr)
        sys.exit(1)
    print("\nresource checks passed")


if __name__ == "__main__":
    main()
