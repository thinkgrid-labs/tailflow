#!/usr/bin/env python3
"""Compare two resource_bench.py result files and flag regressions.

  python3 scripts/bench/compare.py old.jsonl new.jsonl [--old-bin NAME] [--new-bin NAME]
          [--threshold 10] [--markdown]

Both files may come from one interleaved run (pass --old-bin/--new-bin) or
from separate runs. For every (scenario, metric) present in both, prints the
medians, the relative change, and the larger of the two spreads.

A change counts as a regression only when it is worse by more than
--threshold percent AND by more than the observed run-to-run spread AND by
more than the metric's absolute noise floor. That keeps shared-runner noise
from failing a build while still catching real >10 % movements on a
dedicated host. Exit status is 1 when any important metric regressed.
"""

import argparse
import json
import sys

# metric -> (direction, absolute noise floor, important)
# direction +1: higher is worse; -1: lower is worse.
METRICS = {
    "cpu_percent": (1, 0.05, True),
    "wakeups_per_s": (1, 0.5, True),
    "rss_mb_avg": (1, 0.5, True),
    "rss_mb_peak": (1, 0.5, True),
    "private_mb_avg": (1, 0.5, True),
    "private_mb_peak": (1, 0.5, True),
    "quiet_rss_mb_end": (1, 0.5, True),
    "quiet_private_mb_end": (1, 0.5, True),
    "wal_bytes_per_event": (1, 20, True),
    "disk_write_kb_per_s": (1, 5, False),
    "bytes_written_per_event": (1, 50, False),
    "sse_p50_ms": (1, 0.2, False),
    "sse_p95_ms": (1, 0.5, False),
    "sse_p99_ms": (1, 1.0, False),
    "sse_lost_max": (1, 0, True),
    "sse_lost": (1, 0, True),
    "query_p50_ms": (1, 0.5, True),
    "query_p95_ms": (1, 1.0, True),
    "query_p99_ms": (1, 2.0, False),
    "visible_p50_ms": (1, 5, False),
    "visible_p99_ms": (1, 20, False),
    "shutdown_ms": (1, 20, False),
    "fresh_db_health_ms": (1, 10, False),
    "existing_db_first_query_ms": (1, 10, True),
    "rss_mb_after": (1, 0.5, True),
    "rss_mb_after_30s": (1, 0.5, True),
    "db_bytes": (1, 65536, False),
}


def load(path, binary):
    out = {}
    for line in open(path):
        r = json.loads(line)
        if r.get("kind") != "summary":
            continue
        if binary and r["binary"] != binary:
            continue
        out[r["scenario"]] = r["stats"]
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("old")
    ap.add_argument("new")
    ap.add_argument("--old-bin")
    ap.add_argument("--new-bin")
    ap.add_argument("--threshold", type=float, default=10.0)
    ap.add_argument("--markdown", action="store_true")
    a = ap.parse_args()
    old, new = load(a.old, a.old_bin), load(a.new, a.new_bin)
    regressions = []
    rows = []
    for sc in sorted(set(old) & set(new), key=lambda s: list(old).index(s)):
        for m, (direction, floor, important) in METRICS.items():
            if m not in old[sc] or m not in new[sc]:
                continue
            o, n = old[sc][m]["median"], new[sc][m]["median"]
            spread = max(old[sc][m].get("spread_pct", 0), new[sc][m].get("spread_pct", 0))
            delta = n - o
            rel = (delta / o * 100) if o else (0.0 if delta == 0 else float("inf"))
            worse = direction * delta > 0
            regressed = (worse and abs(rel) > a.threshold and abs(rel) > spread and abs(delta) > floor)
            # Loss counters: any increase from zero is a regression.
            if m.startswith("sse_lost") and n > o:
                regressed = True
            verdict = "REGRESSED" if regressed else ("improved" if not worse and abs(rel) > max(a.threshold, spread) and abs(delta) > floor else "same")
            rows.append((sc, m, o, n, rel, spread, verdict, important))
            if regressed and important:
                regressions.append((sc, m, o, n, rel))
    if a.markdown:
        print("| scenario | metric | old | new | Δ % | spread % | verdict |")
        print("|---|---|---|---|---|---|---|")
        for sc, m, o, n, rel, spread, v, _ in rows:
            print(f"| {sc} | {m} | {o:g} | {n:g} | {rel:+.1f} | {spread:.1f} | {v} |")
    else:
        for sc, m, o, n, rel, spread, v, imp in rows:
            print(f"{sc:<22} {m:<26} {o:>12g} {n:>12g} {rel:>+8.1f}% (spread {spread:4.1f}%) {v}{'' if imp else ' (info)'}")
    if regressions:
        print(f"\n{len(regressions)} important regression(s) over {a.threshold}%:", file=sys.stderr)
        for r in regressions:
            print("  %s %s: %g -> %g (%+.1f%%)" % r, file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
