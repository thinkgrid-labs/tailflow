#!/usr/bin/env python3
"""Agent hook smoke test: install, run and uninstall TailFlow's hooks the
way Claude Code and Codex do, natively on the host it runs on (Windows,
macOS, Linux). Needs no agent, account or secret.

  python3 scripts/smoke/agent_hook_smoke.py [--bin-dir target/release] [--out agent-smoke.json]

Standard library only. Everything happens under a fresh temporary directory
whose every path contains spaces and parentheses (`tail flow (x86)`), with
HOME/USERPROFILE, CODEX_HOME and TAILFLOW_DATA_DIR pointing into it:

  1. a user's existing Claude Code settings and Codex hooks are seeded;
  2. `tailflow agent install claude` and `install codex` (from a copy of the
     binary in a directory with spaces and parentheses) keep them intact;
  3. a daemon (`--agents --observe <project>`) starts;
  4. the installed hooks are executed as each agent executes them:
     Claude Code's exec form (`command` + `args`, no shell) and Codex's
     shell form (`cmd /C "<command>"` on Windows, `$SHELL -lc` elsewhere),
     with a session start, a prompt, a successful and a failing command, a
     file edit (Claude `Edit`, Codex `apply_patch`) and a session end;
  5. both sessions, their tool/command events, the failed command's outcome
     and the edited file (path with spaces) reach the API, and the edit is
     matched by an independent filesystem observation;
  6. a sentinel string in command arguments is stored nowhere (API, spool,
     SQLite database and WAL) in the default `metadata` mode;
  7. graceful stop and restart keep both sessions;
  8. `tailflow agent uninstall` removes TailFlow's hooks and only them.

What this proves: TailFlow's installer, hook entry point, quoting, spool,
ingestion, evidence and persistence work natively for both invocation forms.
What it does not prove: that a real Claude Code or Codex build invokes
hooks exactly this way, or sends these payload shapes (they are copied from
the recorded/synthetic fixtures in crates/tailflow-core/tests/fixtures).
That needs the real agents (see docs/internal M4-F reports).
"""

import argparse
import json
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

TOKEN = os.environ.setdefault("TAILFLOW_TOKEN", "tailflow-smoke-token-0123456789abcdef")
AUTH = {"Authorization": f"Bearer {TOKEN}"}
WINDOWS = os.name == "nt"
EXE = ".exe" if WINDOWS else ""
SENTINEL = "TFSMOKE-SENTINEL-7f3a9c"  # must never be stored
LIVE = []


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
        req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=AUTH)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, OSError, ValueError):
        return None


def get_text(port, path, timeout=5):
    try:
        req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=AUTH)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read().decode("utf-8", "replace")
    except (urllib.error.URLError, OSError):
        return ""


def wait_for(fn, timeout, what):
    t0 = time.time()
    while time.time() - t0 < timeout:
        v = fn()
        if v:
            return v
        time.sleep(0.25)
    raise SmokeFailure(f"timed out after {timeout} s waiting for {what}")


def run(cmd, env, **kw):
    return subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60, **kw)


def start_daemon(daemon, env, cwd, project, logdir):
    port = free_port()
    kwargs = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if WINDOWS else {}
    p = subprocess.Popen([daemon, "--agents", "--port", str(port), "--observe", project], cwd=cwd,
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=open(os.path.join(logdir, f"daemon-{port}.log"), "w"),
                         env={**env, "RUST_LOG": "warn"}, **kwargs)
    LIVE.append(p)
    t0 = time.time()
    while time.time() - t0 < 60:
        check(p.poll() is None, f"daemon exited during startup (code {p.returncode})")
        if get(port, "/health", timeout=1):
            return p, port
        time.sleep(0.1)
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
        raise SmokeFailure(f"daemon did not stop within {timeout} s")
    return code, round((time.time() - t0) * 1000)


def handlers(cfg_path, event):
    with open(cfg_path, encoding="utf-8") as f:
        v = json.load(f)
    out = []
    for group in (v.get("hooks") or {}).get(event) or []:
        out.extend(group.get("hooks") or [])
    return v, out


def is_tailflow(h):
    text = " ".join([h.get("command", "")] + list(h.get("args") or []))
    return "agent hook" in text and "tailflow" in text.lower()


def run_claude_hook(h, payload, env):
    # Exec form: Claude Code starts `command` with `args`, without a shell.
    check(h.get("args"), f"Claude handler is not in exec form: {h}")
    r = run([h["command"], *h["args"]], env, input=json.dumps(payload).encode())
    check(r.returncode == 0, f"claude hook exited {r.returncode}: {r.stderr[:300]!r}")
    return r


def run_codex_hook(h, payload, env):
    cmd = h["command"]
    if WINDOWS:
        # Codex on Windows: cmd /C "<command>" (raw command line, so cmd's own
        # quote rules apply, as they do for Codex).
        r = run(f'cmd.exe /C "{cmd}"', env, input=json.dumps(payload).encode())
    else:
        shell = os.environ.get("SHELL") or "/bin/sh"
        r = run([shell, "-lc", cmd], env, input=json.dumps(payload).encode())
    check(r.returncode == 0, f"codex hook exited {r.returncode}: {r.stderr[:300]!r} (command {cmd!r})")
    return r


def session_events(port, sid):
    q = urllib.parse.urlencode({"session": sid, "limit": 500, "order": "asc"})
    page = get(port, f"/api/v1/events?{q}") or {}
    return page.get("events") or []


def types_of(evs):
    return [e.get("event_type") or (e.get("event") or {}).get("event_type") for e in evs]


def flat(e):
    return e.get("event") or e


def scan_files(root, needle):
    hits = []
    b = needle.encode()
    for dp, _, fs in os.walk(root):
        for f in fs:
            p = os.path.join(dp, f)
            try:
                with open(p, "rb") as fh:
                    if b in fh.read():
                        hits.append(p)
            except OSError:
                pass
    return hits


def smoke(bin_dir, report):
    base = tempfile.mkdtemp(prefix="tf agent smoke (x86) ")
    report["workdir"] = base
    home = os.path.join(base, "home dir")
    data = os.path.join(base, "tail flow data")
    project = os.path.join(base, "my project (x86)")
    tool_dir = os.path.join(base, "tail flow (x86)", "bin")
    for d in (home, data, project, tool_dir):
        os.makedirs(d)
    tailflow = os.path.join(tool_dir, "tailflow" + EXE)
    shutil.copy2(os.path.join(bin_dir, "tailflow" + EXE), tailflow)
    daemon = os.path.join(bin_dir, "tailflow-daemon" + EXE)
    env = {k: v for k, v in os.environ.items() if k not in ("CLAUDE_CONFIG_DIR",)}
    env.update({"HOME": home, "USERPROFILE": home, "CODEX_HOME": os.path.join(home, ".codex"),
                "TAILFLOW_DATA_DIR": data})

    # 1. A user's own hooks and settings, which must survive install/uninstall.
    claude_cfg = os.path.join(home, ".claude", "settings.json")
    codex_cfg = os.path.join(home, ".codex", "hooks.json")
    os.makedirs(os.path.dirname(claude_cfg))
    os.makedirs(os.path.dirname(codex_cfg))
    user_claude = {"theme": "dark", "hooks": {"PreToolUse": [
        {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo user-claude-hook"}]}]}}
    user_codex = {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo user-codex-hook"}]}]}}
    for path, v in ((claude_cfg, user_claude), (codex_cfg, user_codex)):
        with open(path, "w", encoding="utf-8") as f:
            json.dump(v, f, indent=2)

    # 2. Install from the copied binary (its path has spaces and parentheses).
    for args in (["install", "claude"], ["install", "codex", "--codex-version", "0.172.0"]):
        r = run([tailflow, "agent", *args], env, cwd=project)
        check(r.returncode == 0, f"tailflow agent {' '.join(args)} failed: {r.stderr[:400]!r}")
    v, claude_pre = handlers(claude_cfg, "PreToolUse")
    check(v.get("theme") == "dark", "Claude settings lost an unrelated key")
    check(any(h.get("command") == "echo user-claude-hook" for h in claude_pre), "user's Claude hook lost")
    claude_h = [h for h in claude_pre if is_tailflow(h)]
    check(len(claude_h) == 1, f"one TailFlow Claude handler expected: {claude_pre}")
    check(os.path.samefile(claude_h[0]["command"], tailflow), f"Claude handler runs {claude_h[0]['command']}")
    _, codex_stop = handlers(codex_cfg, "Stop")
    check(any(h.get("command") == "echo user-codex-hook" for h in codex_stop), "user's Codex hook lost")
    _, codex_start = handlers(codex_cfg, "SessionStart")
    codex_h = [h for h in codex_start if is_tailflow(h)]
    check(len(codex_h) == 1, f"one TailFlow Codex handler expected: {codex_start}")
    report["installed"] = {"claude_handler": claude_h[0], "codex_handler": codex_h[0]}
    status = run([tailflow, "agent", "status", "--json"], env)
    check(status.returncode == 0, f"agent status failed: {status.stderr[:300]!r}")

    # 3. The daemon, observing the project.
    p, port = start_daemon(daemon, env, base, project, base)
    wait_for(lambda: ((get(port, "/api/v1/observe/status") or {}).get("roots") or [{}])[0].get("state")
             == "observing", 60, "observation of the project")
    time.sleep(1.0)

    # 4. Run the hooks as the agents do.
    edited_claude = os.path.join(project, "src dir", "main file.rs")
    edited_codex = os.path.join(project, "src dir", "patched file.rs")
    os.makedirs(os.path.dirname(edited_claude))
    secret_cmd = f"curl -H 'X-Token: {SENTINEL}' https://example.invalid"
    c_sid = "smoke-claude-0001"
    common = {"session_id": c_sid, "cwd": project, "transcript_path": None, "permission_mode": "default"}
    claude_seq = [
        {"hook_event_name": "SessionStart", "source": "startup"},
        {"hook_event_name": "UserPromptSubmit", "prompt": "<smoke prompt>"},
        {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "c-b1",
         "tool_input": {"command": "cargo --version"}},
        {"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "c-b1",
         "tool_input": {"command": "cargo --version"}, "tool_response": {"stdout": "cargo 1.0", "stderr": ""}},
        {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "c-b2",
         "tool_input": {"command": secret_cmd}},
        {"hook_event_name": "PostToolUseFailure", "tool_name": "Bash", "tool_use_id": "c-b2",
         "tool_input": {"command": secret_cmd}, "error": "Exit code 6", "is_interrupt": False},
    ]
    for ev in claude_seq:
        run_claude_hook(claude_h[0], {**common, **ev}, env)
    with open(edited_claude, "w") as f:
        f.write("fn main() {}\n")
    run_claude_hook(claude_h[0], {**common, "hook_event_name": "PostToolUse", "tool_name": "Edit",
                                  "tool_use_id": "c-e1", "tool_input": {"file_path": edited_claude},
                                  "tool_response": {}}, env)

    x_sid = "019a0000-0000-7000-8000-00000000smk1"
    xcommon = {"session_id": x_sid, "cwd": project, "transcript_path": None, "model": "gpt-5.5",
               "permission_mode": "default"}
    _, codex_post = handlers(codex_cfg, "PostToolUse")
    codex_post = [h for h in codex_post if is_tailflow(h)][0]
    run_codex_hook(codex_h[0], {**xcommon, "hook_event_name": "SessionStart", "source": "startup"}, env)
    run_codex_hook(codex_post, {**xcommon, "hook_event_name": "PostToolUse", "turn_id": "t1",
                                "tool_name": "Bash", "tool_use_id": "x-b1",
                                "tool_input": {"command": "git status"}, "tool_response": "clean"}, env)
    with open(edited_codex, "w") as f:
        f.write("// patched\n")
    rel = os.path.relpath(edited_codex, project).replace("\\", "/")
    patch = f"*** Begin Patch\n*** Add File: {rel}\n+// patched\n*** End Patch\n"
    run_codex_hook(codex_post, {**xcommon, "hook_event_name": "PostToolUse", "turn_id": "t1",
                                "tool_name": "apply_patch", "tool_use_id": "x-p1",
                                "tool_input": {"command": patch}, "tool_response": "Success"}, env)
    time.sleep(1.0)
    for hook_list, run_one, payload in (
        (handlers(claude_cfg, "SessionEnd")[1], run_claude_hook, {**common, "hook_event_name": "SessionEnd",
                                                                  "reason": "other"}),
        (handlers(codex_cfg, "SessionEnd")[1], run_codex_hook, {**xcommon, "hook_event_name": "SessionEnd"}),
    ):
        hs = [h for h in hook_list if is_tailflow(h)]
        if hs:
            run_one(hs[0], payload, env)
        else:
            report.setdefault("notes", []).append(f"no SessionEnd handler installed ({run_one.__name__})")

    # 5. What reached the API.
    def ours():
        listed = (get(port, "/api/v1/sessions?limit=50") or {}).get("sessions") or []
        mine = [s for s in listed if c_sid in json.dumps(s) or x_sid in json.dumps(s)]
        return mine if len(mine) >= 2 else None
    sessions = wait_for(ours, 60, "both sessions in /api/v1/sessions")
    sid_of = {}
    for s in sessions:
        sid = s.get("session_id") or s.get("id")
        sid_of["claude" if c_sid in json.dumps(s) else "codex"] = sid
    report["session_ids"] = sid_of
    c_ev = wait_for(lambda: (lambda evs: evs if "agent.session.ended" in types_of(evs) else None)(
        session_events(port, sid_of["claude"])), 60, "the Claude session's events")
    x_ev = wait_for(lambda: (lambda evs: evs if any(t and t.startswith("file.") for t in types_of(evs)) else None)(
        session_events(port, sid_of["codex"])), 60, "the Codex session's file event")
    report["claude_event_types"] = sorted(set(filter(None, types_of(c_ev))))
    report["codex_event_types"] = sorted(set(filter(None, types_of(x_ev))))
    for t in ("agent.session.started", "agent.tool.completed", "agent.tool.failed", "command.failed",
              "file.modified", "agent.session.ended"):
        check(t in report["claude_event_types"], f"Claude session lacks {t}: {report['claude_event_types']}")
    c_files = [flat(e).get("path") for e in c_ev if (types_of([e])[0] or "").startswith("file.")]
    check(any(p and os.path.normcase(p) == os.path.normcase(edited_claude) for p in c_files),
          f"edited path not recorded as {edited_claude!r}: {c_files}")
    x_files = [flat(e).get("path") for e in x_ev if (types_of([e])[0] or "").startswith("file.")]
    check(any(p and os.path.normcase(p) == os.path.normcase(edited_codex) for p in x_files),
          f"apply_patch path not recorded as {edited_codex!r}: {x_files}")
    ev = wait_for(lambda: (lambda v: v if any("main file.rs" in json.dumps(i) for i in (v or {}).get("items") or [])
                           else None)(get(port, "/api/v1/evidence/files?since=30m")), 60, "file evidence")
    statuses = {i.get("path"): i.get("status") for i in ev.get("items") or []}
    report["evidence"] = statuses
    st = next((s for p_, s in statuses.items() if p_ and os.path.normcase(p_) == os.path.normcase(edited_claude)), None)
    check(st == "matched_observation", f"Claude edit evidence is {st!r}: {statuses}")

    # 6. Privacy: command arguments are not stored in metadata mode.
    api_text = "".join(get_text(port, f"/api/v1/events?{urllib.parse.urlencode({'session': sid_of[k], 'limit': 500})}")
                       for k in sid_of)
    check(SENTINEL not in api_text, "a command argument reached the API")

    # 7. Graceful stop, restart, sessions persist.
    code, ms = graceful_stop(p)
    LIVE.remove(p)
    check(code == 0, f"graceful stop exited {code}")
    report["shutdown_ms"] = ms
    leaks = scan_files(data, SENTINEL)
    check(not leaks, f"a command argument was stored in {leaks}")
    p2, port2 = start_daemon(daemon, env, base, project, base)
    again = (get(port2, "/api/v1/sessions?limit=50") or {}).get("sessions") or []
    check(sum(1 for s in again if c_sid in json.dumps(s) or x_sid in json.dumps(s)) >= 2,
          "sessions missing after restart")
    code, _ = graceful_stop(p2)
    LIVE.remove(p2)
    check(code == 0, f"second graceful stop exited {code}")

    # 8. Uninstall removes ours only.
    for agent in ("claude", "codex"):
        r = run([tailflow, "agent", "uninstall", agent], env, cwd=project)
        check(r.returncode == 0, f"uninstall {agent} failed: {r.stderr[:300]!r}")
    v, pre = handlers(claude_cfg, "PreToolUse")
    check(v.get("theme") == "dark" and any(h.get("command") == "echo user-claude-hook" for h in pre),
          "uninstall damaged the user's Claude settings")
    check(not any(is_tailflow(h) for evn in (v.get("hooks") or {}) for h in handlers(claude_cfg, evn)[1]),
          "a TailFlow Claude handler survived uninstall")
    v, stop = handlers(codex_cfg, "Stop")
    check(any(h.get("command") == "echo user-codex-hook" for h in stop), "uninstall damaged the user's Codex hooks")
    check(not any(is_tailflow(h) for evn in (v.get("hooks") or {}) for h in handlers(codex_cfg, evn)[1]),
          "a TailFlow Codex handler survived uninstall")

    # Spool permissions (informational on Windows).
    spool = os.path.join(data, "agents", "spool")
    if WINDOWS:
        r = run(["icacls", spool], env)
        report["spool_acl"] = r.stdout.decode("utf-8", "replace")[:600]
    elif os.path.isdir(spool):
        mode = os.stat(spool).st_mode & 0o777
        report["spool_mode"] = oct(mode)
        check(mode & 0o077 == 0, f"spool directory is accessible to others: {oct(mode)}")
    shutil.rmtree(base, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin-dir", default=os.path.join("target", "release"))
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    report = {"platform": platform.platform(), "python": sys.version.split()[0]}
    ok = False
    try:
        smoke(os.path.abspath(a.bin_dir), report)
        ok = True
    except SmokeFailure as e:
        report["failure"] = str(e)
    finally:
        for p in LIVE:
            if p.poll() is None:
                p.kill()
    report["ok"] = ok
    text = json.dumps(report, indent=2, default=str)
    if a.out:
        with open(a.out, "w") as f:
            f.write(text)
    print(text)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
