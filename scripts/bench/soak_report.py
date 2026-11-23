#!/usr/bin/env python3
"""Summarize a `resource_bench.py --scenarios soak:<minutes>` run.

  python3 scripts/bench/soak_report.py soak.jsonl [--markdown]

Per hour: RSS / private memory (min, max, mean, end), CPU, live events, DB
and WAL size, loss counters, query latency by kind (p50/p95/p99/max,
failures) and retention pass duration. Plus the malloc-zone and FTS samples,
and memory slopes fitted over the last half and the last quarter of the run.
A run "plateaus" when the late slopes are within noise of zero; it is never
judged by an absolute ceiling alone.
"""

import argparse
import json
import statistics


def pct(v, p):
    if not v:
        return None
    v = sorted(v)
    return round(v[min(len(v) - 1, int(round((len(v) - 1) * p)))], 2)


def slope(points):
    """Least-squares slope in units per hour of (t_seconds, value) points."""
    if len(points) < 3:
        return None
    xs = [t / 3600 for t, _ in points]
    ys = [v for _, v in points]
    mx, my = statistics.mean(xs), statistics.mean(ys)
    den = sum((x - mx) ** 2 for x in xs)
    return round(sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den, 3) if den else None


def analyse(run):
    series, qlog = run.get("series") or [], run.get("query_log") or []
    end = max((s["t"] for s in series), default=0)
    hours = int(end // 3600) + 1
    rows = []
    prev_cpu = None
    for h in range(hours):
        lo, hi = h * 3600, (h + 1) * 3600
        ss = [s for s in series if lo <= s["t"] < hi]
        if not ss:
            continue
        qs = [q for q in qlog if lo <= q[0] < hi]
        passes = [p for p in run.get("retention_passes") or [] if lo <= p[0] < hi]
        cpu_end = ss[-1]["cpu_s"]
        cpu_start = prev_cpu if prev_cpu is not None else ss[0]["cpu_s"]
        span = (ss[-1]["t"] - (lo if prev_cpu is not None else ss[0]["t"])) or 1
        prev_cpu = cpu_end
        row = {
            "hour": h + 1,
            "rss_min": min(s["rss_mb"] for s in ss), "rss_max": max(s["rss_mb"] for s in ss),
            "rss_mean": round(statistics.mean(s["rss_mb"] for s in ss), 1), "rss_end": ss[-1]["rss_mb"],
            "priv_min": min(s["private_mb"] for s in ss), "priv_max": max(s["private_mb"] for s in ss),
            "priv_mean": round(statistics.mean(s["private_mb"] for s in ss), 1), "priv_end": ss[-1]["private_mb"],
            "cpu_pct": round(100 * (cpu_end - cpu_start) / span, 2),
            "live_events_min": min((s.get("live_events") or 0) for s in ss),
            "live_events_max": max((s.get("live_events") or 0) for s in ss),
            "db_mb_max": max(s["db_mb"] for s in ss), "wal_mb_max": max(s["wal_mb"] for s in ss),
            "dropped_end": ss[-1].get("dropped"), "ingest_dropped_end": ss[-1].get("ingest_dropped"),
            "sse_lag_end": ss[-1].get("sse_lag"), "storage_errors_end": ss[-1].get("storage_errors"),
            "queue_max": max((s.get("queue") or 0) for s in ss),
            "queries": len(qs), "query_failed": sum(1 for q in qs if not q[3]),
            "retention_passes": len(passes),
            "retention_ms_p50": pct([p[1] for p in passes if p[1] is not None], 0.5),
            "retention_ms_max": pct([p[1] for p in passes if p[1] is not None], 1.0),
        }
        for kind in sorted({q[1] for q in qs}):
            lat = [q[2] for q in qs if q[1] == kind and q[3]]
            row[f"q_{kind}"] = {"n": len(lat), "p50": pct(lat, 0.5), "p95": pct(lat, 0.95),
                                "p99": pct(lat, 0.99), "max": pct(lat, 1.0)}
        rows.append(row)
    half = [s for s in series if s["t"] >= end / 2]
    quarter = [s for s in series if s["t"] >= end * 3 / 4]
    summary = {
        "minutes": run.get("minutes"), "cycles": run.get("cycles"), "daemon_alive": run.get("daemon_alive"),
        "generated": run.get("generated"), "invocations": run.get("generator_invocations"),
        "ring_cursor": run.get("ingested_ring_cursor"),
        "cursor_exact": run.get("ingested_ring_cursor") == (run.get("generated") or 0) + (run.get("generator_invocations") or 0),
        "persisted": run.get("persisted"), "storage_dropped": run.get("storage_dropped"),
        "ingest_dropped": run.get("ingest_dropped_reported"), "sse_gap_events": run.get("sse_gap_events"),
        "sse_probe_errors": run.get("sse_probe_errors"), "queries": run.get("queries"),
        "query_failed": run.get("query_failed"), "integrity": run.get("integrity"),
        "cpu_percent": run.get("cpu_percent"), "disk_write_mb": run.get("disk_write_mb"),
        "shutdown_ms": run.get("shutdown_ms"),
        "rss_slope_last_half_mb_h": slope([(s["t"], s["rss_mb"]) for s in half]),
        "rss_slope_last_quarter_mb_h": slope([(s["t"], s["rss_mb"]) for s in quarter]),
        "private_slope_last_half_mb_h": slope([(s["t"], s["private_mb"]) for s in half]),
        "private_slope_last_quarter_mb_h": slope([(s["t"], s["private_mb"]) for s in quarter]),
        "private_max_by_quarter": [max((s["private_mb"] for s in series if q * end / 4 <= s["t"] < (q + 1) * end / 4), default=None)
                                   for q in range(4)] if end else [],
    }
    return summary, rows, run.get("heap") or []


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("files", nargs="+")
    ap.add_argument("--markdown", action="store_true")
    a = ap.parse_args()
    for path in a.files:
        for line in open(path):
            r = json.loads(line)
            if r.get("kind") != "run" or not str(r.get("scenario", "")).startswith("soak"):
                continue
            summary, rows, heap = analyse(r)
            print(f"# {path} · {r['binary']} · {r['scenario']}")
            print(json.dumps(summary, indent=1))
            if a.markdown:
                cols = ["hour", "rss_min", "rss_max", "rss_end", "priv_min", "priv_max", "priv_end", "cpu_pct",
                        "live_events_min", "live_events_max", "db_mb_max", "wal_mb_max", "queue_max",
                        "dropped_end", "query_failed", "retention_ms_p50", "retention_ms_max"]
                print("| " + " | ".join(cols) + " |")
                print("|" + "---|" * len(cols))
                for row in rows:
                    print("| " + " | ".join(str(row.get(c)) for c in cols) + " |")
                kinds = sorted({k for row in rows for k in row if k.startswith("q_")})
                print("\n| hour | " + " | ".join(f"{k[2:]} p50/p95/p99" for k in kinds) + " |")
                print("|---|" + "---|" * len(kinds))
                for row in rows:
                    cells = []
                    for k in kinds:
                        q = row.get(k) or {}
                        cells.append(f"{q.get('p50')}/{q.get('p95')}/{q.get('p99')}")
                    print(f"| {row['hour']} | " + " | ".join(cells) + " |")
                if heap:
                    hc = ["t", "zone_allocated_mb", "zone_dirty_mb", "zone_frag_mb", "zone_allocations",
                          "live_events", "fts_data_mb", "fts_bytes_per_live_event"]
                    print("\n| " + " | ".join(hc) + " |")
                    print("|" + "---|" * len(hc))
                    for h in heap:
                        print("| " + " | ".join(str(h.get(c)) for c in hc) + " |")
            else:
                for row in rows:
                    print(json.dumps(row))
                for h in heap:
                    print(json.dumps(h))


if __name__ == "__main__":
    main()
