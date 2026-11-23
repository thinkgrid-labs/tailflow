#!/usr/bin/env python3
"""Self-test for the CI resource gate (check_budgets.py).

  python3 scripts/bench/gate_selftest.py [--target-dir target] [--unit-only]

1. Unit: feeds `evaluate()` and `check_sizes()` synthetic results — a crashed
   daemon, a failed or garbled SSE probe, each kind of loss, missing metrics,
   an oversized binary — and requires a failure for each, plus a pass for a
   clean result.
2. End to end: runs the real gate against the release daemon once clean
   (must pass) and once per injected fault (`--inject`), each of which must
   exit non-zero with the expected message.

Exits non-zero if the gate ever passes something it must fail.
"""

import argparse
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import check_budgets as cb  # noqa: E402

MEM = {"idle_peak": 32, "load_peak": 96, "after_load": 64}
CLEAN = {
    "daemon_alive": True, "shutdown_ms": 20.0, "generated": 10000,
    "sse_received_min": 10000, "sse_lost_max": 0, "ingest_dropped_reported": 0,
    "storage_dropped": 0, "ingested_ring_cursor": 10001, "persisted": 10004,
    "rss_mb_peak": 30.0, "quiet_rss_mb_end": 25.0,
}

UNIT_CASES = [
    # (description, overrides, expected substring in some failure)
    ("daemon crashed", {"daemon_alive": False, "shutdown_ms": None}, "daemon exited"),
    ("daemon killed at shutdown", {"shutdown_ms": None}, "no value for shutdown_ms"),
    ("SSE probe produced nothing", {"sse_received_min": 0, "sse_lost_max": None}, "received nothing"),
    ("SSE loss", {"sse_lost_max": 3}, "SSE client lost records"),
    ("ingest loss", {"ingest_dropped_reported": 7}, "ingestion dropped"),
    ("storage loss", {"storage_dropped": 1}, "storage dropped events"),
    ("ring cursor short", {"ingested_ring_cursor": 9999}, "ring cursor"),
    ("persisted short", {"persisted": 10000}, "fewer events persisted"),
    ("storage metrics missing", {"storage_dropped": None, "persisted": None}, "no value for storage_dropped"),
    ("health missing", {"ingested_ring_cursor": None, "ingest_dropped_reported": None}, "no value for ingested_ring_cursor"),
    ("memory sampler empty", {"rss_mb_peak": None}, "no value for rss_mb_peak"),
    ("memory ceiling", {"rss_mb_peak": 500.0}, "peak RSS"),
    ("generated missing", {"generated": None}, "no generated count"),
]

E2E_CASES = [
    # (fault, expected substring)
    ("crash", "daemon exited during the run"),
    ("probe", "received nothing"),
    ("probe_garbage", "received nothing"),
    ("loss", "storage dropped events"),
    ("missing", "no value for storage_dropped"),
    ("size", "exceeds budget"),
]


def unit():
    bad = []
    if cb.evaluate("load_1000", dict(CLEAN), MEM):
        bad.append(f"clean result failed: {cb.evaluate('load_1000', dict(CLEAN), MEM)}")
    for desc, over, want in UNIT_CASES:
        r = {**CLEAN, **over}
        r = {k: v for k, v in r.items() if v is not None}
        fails = cb.evaluate("load_1000", r, MEM)
        if not any(want in f for f in fails):
            bad.append(f"{desc}: expected a failure containing {want!r}, got {fails}")
    # Burst results report `sse_lost` instead of `sse_lost_max`.
    burst = {k: v for k, v in CLEAN.items() if k != "sse_lost_max"}
    if not any("sse_lost" in f for f in cb.evaluate("burst_5000", burst, MEM)):
        bad.append("burst without sse_lost passed")
    if not cb.evaluate("idle", {"daemon_alive": True, "shutdown_ms": 1.0}, MEM):
        bad.append("idle without memory reading passed")
    # Sizes: an oversized binary, a missing binary, and a missing budget.
    with tempfile.TemporaryDirectory() as d:
        for name in ("tailflow", "tailflow-daemon", "tailflow-mcp", "tailflow-logs"):
            with open(os.path.join(d, name + (".exe" if os.name == "nt" else "")), "wb") as f:
                f.write(b"x" * 1000)
        ok = {"binary_headroom_pct": 10, "binary_bytes": {"t": {n: 1000 for n in
              ("tailflow", "tailflow-daemon", "tailflow-mcp", "tailflow-logs")}}}
        if cb.check_sizes(d, ok, "t", True)[0]:
            bad.append("in-budget binaries failed")
        over = {**ok, "binary_bytes": {"t": {**ok["binary_bytes"]["t"], "tailflow-daemon": 500}}}
        if not any("exceeds budget" in f for f in cb.check_sizes(d, over, "t", True)[0]):
            bad.append("oversized binary passed")
        if not any("no size budget" in f for f in cb.check_sizes(d, ok, "other", True)[0]):
            bad.append("missing budget passed with --require-size-budget")
        os.remove(os.path.join(d, "tailflow-mcp" + (".exe" if os.name == "nt" else "")))
        if not any("missing release binary" in f for f in cb.check_sizes(d, ok, "t", False)[0]):
            bad.append("missing binary passed")
    return bad


def e2e(target_dir):
    bad = []
    gate = os.path.join(HERE, "check_budgets.py")
    base = [sys.executable, "-I", gate, "--target-dir", target_dir, "--only", "load_1000"]
    clean = subprocess.run(base, capture_output=True, text=True, timeout=600)
    print(f"clean: exit {clean.returncode}", flush=True)
    if clean.returncode != 0:
        bad.append(f"clean gate run failed:\n{clean.stderr[-2000:]}")
    for fault, want in E2E_CASES:
        r = subprocess.run(base + ["--inject", fault], capture_output=True, text=True, timeout=600)
        hit = want in r.stderr
        print(f"inject {fault}: exit {r.returncode}, expected message {'found' if hit else 'MISSING'}", flush=True)
        if r.returncode == 0:
            bad.append(f"inject {fault}: the gate PASSED")
        elif not hit:
            bad.append(f"inject {fault}: failed, but without {want!r}:\n{r.stderr[-2000:]}")
    return bad


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target-dir", default=os.path.join(cb.rb.ROOT, "target"))
    ap.add_argument("--unit-only", action="store_true")
    a = ap.parse_args()
    bad = unit()
    print(f"unit: {len(UNIT_CASES) + 6} cases, {len(bad)} wrong", flush=True)
    if not a.unit_only:
        bad += e2e(a.target_dir)
    if bad:
        print("\ngate self-test FAILED:", file=sys.stderr)
        for b in bad:
            print("  " + b, file=sys.stderr)
        sys.exit(1)
    print("\ngate self-test passed")


if __name__ == "__main__":
    main()
