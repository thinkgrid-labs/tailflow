#!/usr/bin/env python3
"""Release smoke test for one platform (Linux, macOS, Windows).

  python3 scripts/smoke/platform_smoke.py [--bin target/release/tailflow-daemon] [--out smoke.json]

Standard library only. Runs the release daemon in a fresh directory and checks
what a user relies on, natively on the host it runs on:

  1. the daemon starts and the local HTTP API answers;
  2. the SQLite database is created;
  3. file collection: lines appended to a tailed file are captured;
  4. process collection: a configured command's output is captured;
  5. everything captured is persisted (history API), with nothing dropped;
  6. graceful shutdown (SIGTERM on Unix, Ctrl+Break on Windows) with a live
     SSE client connected exits 0 and closes the run cleanly;
  7. after a restart the first run's events are still there and history is
     reported complete (no unclean-shutdown gap);
  8. after a hard kill, the next start records the loss as a gap — on Windows,
     where there is no cheap process-liveness probe, only once the dead run's
     heartbeat is stale (60 s); the result is reported either way.

Exits non-zero on the first failed check of 1–7; 8 is asserted on Unix only.
"""

import argparse
import json
import os
import platform
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

# Daemons from M4 require an API token. Every daemon this script starts
# inherits this one through the environment (older daemons ignore it).
TOKEN = os.environ.setdefault("TAILFLOW_TOKEN", "tailflow-bench-token-0123456789abcdef")
AUTH = {"Authorization": f"Bearer {TOKEN}"}

WINDOWS = os.name == "nt"
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FILE_LINES = 50
LIVE = []  # every daemon started, killed on any unexpected exit


class SmokeFailure(Exception):
    pass


def check(cond, msg):
    if not cond:
        raise SmokeFailure(msg)


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
    except (urllib.error.URLError, OSError, ValueError):
        return None


def toml_literal(path):
    # TOML literal strings take Windows backslashes verbatim.
    check("'" not in path, f"path with a quote: {path}")
    return f"'{path}'"


def write_config(d):
    log = os.path.join(d, "app.log")
    open(log, "w").close()
    cfg = os.path.join(d, "tailflow.toml")
    with open(cfg, "w") as f:
        f.write("[[sources.process]]\nlabel = \"proc\"\ncmd = \"echo smoke-process-hello\"\n\n")
        f.write(f"[[sources.file]]\nlabel = \"file\"\npath = {toml_literal(log)}\n\n")
        f.write(f"[storage]\npath = {toml_literal(os.path.join(d, 'events.db'))}\n")
    return cfg, log, os.path.join(d, "events.db")


def start(binary, cfg, d):
    port = free_port()
    kwargs = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if WINDOWS else {}
    p = subprocess.Popen([binary, "--config", cfg, "--port", str(port)], cwd=d,
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=open(os.path.join(d, f"daemon-{port}.log"), "w"),
                         env={**os.environ, "RUST_LOG": "warn", "TAILFLOW_DATA_DIR": d}, **kwargs)
    LIVE.append(p)
    t0 = time.time()
    while time.time() - t0 < 60:
        check(p.poll() is None, f"daemon exited during startup (code {p.returncode})")
        if get(port, "/health", timeout=1):
            return p, port, round((time.time() - t0) * 1000)
        time.sleep(0.05)
    p.kill()
    raise SmokeFailure("daemon did not answer /health within 60 s")


def graceful_stop(p, timeout=30):
    t0 = time.time()
    if WINDOWS:
        os.kill(p.pid, signal.CTRL_BREAK_EVENT)
    else:
        p.send_signal(signal.SIGTERM)
    try:
        code = p.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        raise SmokeFailure(f"daemon did not stop within {timeout} s of a graceful stop request")
    return code, round((time.time() - t0) * 1000)


def events(port, source, limit=500):
    q = urllib.parse.urlencode({"source": source, "limit": limit})
    return get(port, f"/api/v1/events?{q}") or {}


def wait_for(fn, timeout, what):
    t0 = time.time()
    last = None
    while time.time() - t0 < timeout:
        last = fn()
        if last:
            return last
        time.sleep(0.25)
    raise SmokeFailure(f"timed out after {timeout} s waiting for {what}")


def run(binary, report):
    d = tempfile.mkdtemp(prefix="tailflow-smoke-")
    report["workdir"] = d
    cfg, log, db = write_config(d)

    # 1–2. Start, API, database.
    p, port, startup_ms = start(binary, cfg, d)
    report["startup_ms"] = startup_ms
    status = get(port, "/api/v1/storage/status")
    check(status and status.get("enabled"), f"storage not enabled: {status}")
    check(os.path.exists(db), f"database not created at {db}")
    report["sqlite_version"] = (status.get("db") or {}).get("sqlite_version")

    # 3. File collection.
    for i in range(FILE_LINES):
        with open(log, "a", encoding="utf-8", newline="\n") as f:
            f.write(f"smoke-file-line {i} level=info\n")
        if i % 10 == 9:
            time.sleep(0.1)
    file_events = wait_for(lambda: (lambda e: e if len(e) >= FILE_LINES else None)(
        [x for x in events(port, "file").get("events", []) if "smoke-file-line" in (x.get("message") or "")]),
        30, f"{FILE_LINES} file lines in history")
    check(len(file_events) == FILE_LINES, f"expected {FILE_LINES} file events, got {len(file_events)}")
    report["file_events"] = len(file_events)

    # 4. Process collection.
    wait_for(lambda: [x for x in events(port, "proc").get("events", [])
                      if "smoke-process-hello" in (x.get("message") or "")], 30, "the process line in history")
    report["process_capture"] = True

    # 5. Nothing dropped.
    h = (get(port, "/api/v1/storage/status") or {}).get("health") or {}
    check(h.get("events_dropped_total") == 0, f"storage dropped events: {h}")
    check(h.get("writer_state") == "ok", f"writer not ok: {h}")
    report["persisted_run1"] = h.get("events_persisted")

    # 6. Graceful shutdown — with a live SSE client connected, as when a
    # dashboard tab is open (the stream must end so the server can stop).
    sse = socket.create_connection(("127.0.0.1", port), timeout=5)
    sse.sendall(f"GET /events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n".encode())
    check(sse.recv(64).startswith(b"HTTP/1.1 200"), "SSE stream did not open")
    report["sse_client_connected_at_stop"] = True
    code, ms = graceful_stop(p)
    sse.close()
    report["graceful_exit_code"], report["graceful_shutdown_ms"] = code, ms
    check(code == 0, f"graceful stop exited with {code}")

    # 7. Restart: history survives and is complete.
    p, port, _ = start(binary, cfg, d)
    page = events(port, "file")
    survived = [x for x in page.get("events", []) if "smoke-file-line" in (x.get("message") or "")]
    check(len(survived) == FILE_LINES, f"after restart {len(survived)}/{FILE_LINES} file events remain")
    st = get(port, "/api/v1/storage/status") or {}
    report["restart_recovered_unclean_runs"] = st.get("recovered_unclean_runs")
    check(st.get("recovered_unclean_runs") == 0, f"clean stop was recorded as unclean: {st.get('recovered_unclean_runs')}")
    comp = page.get("completeness") or {}
    report["restart_history_complete"] = comp.get("complete")
    check(comp.get("complete") is True, f"history not complete after a clean restart: {comp}")

    # 8. Hard kill → next start records the loss.
    p.kill()
    p.wait()
    p, port, _ = start(binary, cfg, d)
    st = get(port, "/api/v1/storage/status") or {}
    recovered = st.get("recovered_unclean_runs") or 0
    report["hard_kill_detected_at_start"] = recovered >= 1
    if WINDOWS and recovered == 0:
        # No pid probe on Windows: the dead run is closed only once its
        # heartbeat is older than 60 s (startup or a later liveness pass).
        # Documented limitation; recorded, not asserted.
        report["windows_liveness_note"] = "no pid probe; detection waits for heartbeat staleness (60 s)"
    elif not WINDOWS:
        check(recovered >= 1, f"hard kill not recorded as an unclean run: {st.get('recovered_unclean_runs')}")
    code, _ = graceful_stop(p)
    check(code == 0, f"graceful stop after recovery exited with {code}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    exe = "tailflow-daemon.exe" if WINDOWS else "tailflow-daemon"
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", exe))
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    report = {"platform": platform.platform(), "machine": platform.machine(), "python": platform.python_version(),
              "binary": a.bin, "binary_bytes": os.path.getsize(a.bin) if os.path.exists(a.bin) else None}
    ok = True
    try:
        check(os.path.exists(a.bin), f"missing binary {a.bin}")
        run(a.bin, report)
    except SmokeFailure as e:
        ok = False
        report["failure"] = str(e)
    except Exception as e:  # never lose the report or leave a daemon behind
        ok = False
        report["failure"] = f"{type(e).__name__}: {e}"
    finally:
        for p in LIVE:
            if p.poll() is None:
                p.kill()
                p.wait()
    report["passed"] = ok
    text = json.dumps(report, indent=2)
    print(text)
    if a.out:
        with open(a.out, "w") as f:
            f.write(text + "\n")
    if not ok:
        logs = [f for f in os.listdir(report.get("workdir", ".")) if f.startswith("daemon-")] if report.get("workdir") else []
        for name in logs:
            with open(os.path.join(report["workdir"], name), errors="replace") as f:
                print(f"--- {name}\n{f.read()[-4000:]}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
