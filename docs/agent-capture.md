# Agent capture: recording what your coding agent does

> **Status:** on `dev`, ships in the next release. Supported agents: **Claude
> Code** (validated live against 2.1.331) and **OpenAI Codex** (0.124+, built
> from the published hook schemas; not yet run against a live Codex).

TailFlow can record a coding agent's own activity: its sessions, the tools it
calls, the shell commands it runs, and the files its tools report reading or
editing. You install a small observe-only hook into the agent's own
configuration, keep working as usual, and inspect the history afterwards.

Agent capture is **opt-in**: nothing is recorded until you run
`tailflow agent install`. It needs no account, runs no extra background
service, and sends nothing off your machine.

## Quick start

```bash
# 1. Install the hooks (edits ~/.claude/settings.json; makes a backup)
tailflow agent install claude

# 2. Start a new Claude Code session and work normally.

# 3. Look at what was recorded (works with or without a running daemon)
tailflow sessions
tailflow session <id> --events
```

For Codex:

```bash
tailflow agent install codex   # writes ~/.codex/hooks.json
codex                          # then run /hooks and trust the TailFlow hooks
```

Codex runs a hook only after you review and trust it in `/hooks`, and asks
again whenever the hook changes. TailFlow cannot (and does not) trust its own
hooks on your behalf.

## How it works

```text
agent ──hook──▶ tailflow agent hook <agent>     (one short process per hook call)
                 │ validate, normalize, redact
                 ▼
                 <data dir>/agents/spool/*.json  (owner-only files)
                 │
   any running tailflow-daemon (or `tailflow sessions`) ingests them
                 ▼
                 SQLite history: events + sessions
```

- The hook prints nothing, always exits 0, and never blocks or changes a tool
  call. It does not open the database or the network.
- If no daemon is running, events wait in the spool. The next daemon start, or
  `tailflow sessions`, ingests them. `tailflow agent status` shows what is
  waiting.
- Run a daemon continuously with `tailflow-daemon --agents` (no log sources
  needed). A daemon started for your stack (`tailflow-daemon` with a
  `tailflow.toml`) ingests agent events too.
- Agent events go into the **default per-user database** only. A daemon
  configured with a custom `[storage] path` or `--db` leaves them for one that
  uses the default (its startup banner says so), so one session is never split
  across two databases.

The data directory is `~/Library/Application Support/tailflow` (macOS),
`$XDG_DATA_HOME/tailflow` or `~/.local/share/tailflow` (Linux), or
`%LOCALAPPDATA%\tailflow` (Windows). `TAILFLOW_DATA_DIR` overrides it. The hook
and the daemon must see the same value.

## Commands

| Command | What it does |
|---|---|
| `tailflow agent install claude [--scope user\|project\|local]` | Adds TailFlow's hooks to Claude Code settings. `user` (default) is `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`), `project` is `.claude/settings.json`, `local` is `.claude/settings.local.json`. |
| `tailflow agent install codex [--codex-version X.Y.Z]` | Adds hooks to `~/.codex/hooks.json` (or `$CODEX_HOME`). Only events your Codex version supports are registered; the version is read from `codex --version`. |
| `… --dry-run` | Prints the resulting configuration without writing it. |
| `… --config FILE` | Edits a specific file instead. |
| `tailflow agent status` | What is installed where, missing events, policy blockers, the capture mode, and the spool. |
| `tailflow agent list` | Each integration's capability matrix. |
| `tailflow agent uninstall claude\|codex` | Removes TailFlow's hooks, and only TailFlow's. Recorded history is kept. |
| `tailflow sessions [--agent A] [--project DIR] [--status S] [--since 2h] [--json]` | Lists sessions, newest first. Ingests the spool first unless `--no-ingest`. |
| `tailflow session <id> [--events] [--json]` | One session: lifecycle, counts, tools, commands, files, failures, coverage. `<id>` may be a unique prefix. |
| `tailflow-daemon agent ingest` | Move spooled events into history now. |
| `tailflow-daemon storage purge --session <id> --yes` | Delete one session's events. |

The same data is available over the daemon's read-only HTTP API:
`GET /api/v1/sessions`, `GET /api/v1/sessions/{id}`, `GET /api/v1/agents/status`,
and `GET /api/v1/events?session=<id>` for the chronological events.

### Installer guarantees

- Your other settings and other tools' hooks are left exactly as they are, in
  the same order. TailFlow adds its **own** matcher group per event.
- Re-running install is safe: it updates TailFlow's entries (for example after
  moving the binary) and never duplicates them.
- Each write first saves `<file>.tailflow-backup`, then replaces the file
  atomically, keeping its permissions. A symlinked settings file (dotfile
  managers) is updated at its target.
- A file that is not valid JSON, has an unexpected `hooks` shape, is read-only,
  or changes while TailFlow is editing it is refused and left untouched.
- Claude Code gets the exec form (`command` + `args`): no shell parses the
  path. Codex only supports a shell command, so the path is quoted for the
  platform's hook shell.
- Install hooks from a stable binary location. `npx` caches and build
  directories move; `install` warns when the path looks temporary.

## What is recorded

Every event is `agent_reported`: the agent's hook said it happened. TailFlow
does not verify it independently.

| Event | Claude Code | Codex |
|---|---|---|
| `agent.session.started` / `ended` | SessionStart (startup, resume, clear, compact) / SessionEnd with reason | SessionStart / SessionEnd (0.145+; reason is always `other`) |
| `agent.prompt.submitted` | UserPromptSubmit: character count only | UserPromptSubmit: character count only |
| `agent.turn.completed` / `failed` | Stop / StopFailure (error kind) | Stop |
| `agent.tool.started` / `completed` / `failed` | Every tool incl. MCP, paired by `tool_use_id`, with `duration_ms` | Bash, apply_patch, MCP and local function tools; **no failure event**; no duration |
| `command.started` / `completed` / `failed` | Bash/PowerShell; exit code only on failure (from the error text) | Bash; end event **only if** the response carries an exit code (undocumented), else `command.outcome = unknown` |
| `file.read` | Read tool | — (no read tool) |
| `file.created` / `modified` | Write (create vs update), Edit, MultiEdit, NotebookEdit | apply_patch Add/Update |
| `file.deleted` / `renamed` | — | apply_patch Delete / Move |
| `agent.subagent.started` / `stopped` | SubagentStart / SubagentStop; the subagent's tool calls link to it | SubagentStart / SubagentStop (0.133+) |
| `agent.model.completed` | — | PostModelRequest (0.171+): token counts per attempt |

Sessions record the project (Git working tree, or the working directory), the
Git branch and HEAD commit at start and end (read from `.git` files, never by
running `git`), the model when reported, and counts.

### What TailFlow cannot see

- **Shell side effects.** A file written by `echo > f`, `sed -i`, `mv` or a
  build script is not reported by any file tool, so it is not recorded.
  `file.*` events mean "the agent's tool reported this", not "TailFlow saw the
  file change".
- **Commands inside commands.** Only the command line the agent ran is
  recorded, not what the scripts it started did.
- **Who moved HEAD.** If HEAD differs between session start and end, the
  session shows `inferred_head_changed`. The developer may have committed in
  another terminal; it is not attributed to the agent.
- **Tokens and cost for Claude Code.** Hook input carries no usage. TailFlow
  does not parse Claude Code transcripts (an internal format). Codex token
  counts come from its documented `PostModelRequest` hook. TailFlow never
  estimates cost.
- **Agent versions.** Neither agent puts its version in hook input.
- **Remote agents.** Claude Code on the web and other cloud-run agents have no
  local hooks.
- **Codex hosted tools** (web search) are not hooked.

### Is a session complete?

Each session carries `coverage.history`:

- `complete`: start and end were observed, no loss was recorded during it, and
  none of its events have expired. This is complete *within the integration's
  capabilities* (see `integration_limits`).
- `partial`: one of those is missing. `coverage.reasons` says which:
  `start_not_observed`, `end_not_observed`, `session_in_progress`,
  `retention_truncated`, `loss_recorded`.
- `unknown`: neither start nor end was observed (for example, hooks were
  installed mid-session).

Status is `active` (no end yet, recent activity), `ended`, or `unterminated`
(no end observed and no activity for 30 minutes: crashed, killed, or idle;
TailFlow cannot tell which).

## Privacy

Agent activity can reveal source code, commands, paths and credentials, so
capture is **metadata-first**:

- **Never recorded** in any mode: prompt text, assistant messages, tool
  output, file contents, transcript contents.
- **Shell commands:** by default only the program name (`cargo`, `npm`) and
  shape (length, whether it is compound). The full command line is stored only
  in `redacted` (after secret redaction) or `full` capture mode.
- **Error text** of failed tools is stored only in `redacted`/`full` mode.
- Redaction and collection boundaries run **in the hook, before anything is
  written to disk**, and again in the daemon.

Configure agent capture in `<data dir>/agents/config.toml`, with the same keys
as `[privacy]` in `tailflow.toml`:

```toml
[privacy]
capture = "metadata"                     # metadata | redacted | full
allowed_project_dirs = ["/Users/me/work"] # record nothing outside these
excluded_file_globs = ["**/.env", "**/.env.*", "**/secrets/**"]
excluded_agents = []                     # claude-code (or claude), codex
```

An invalid config makes the hook record nothing (counted as loss and shown by
`tailflow agent status`) rather than record under a policy you did not
intend. Paths are normalized (`..` resolved) before the allowlist and globs
are checked; symlinks are not resolved.

The spool directory is owner-only (`0700`/`0600` on Unix; on Windows it lives
in your profile's `%LOCALAPPDATA%`). Anyone who can run programs as your user
can write to it, exactly as they could write to the database itself: agent
events are trusted at that boundary and no further.

## Loss and failure behavior

- **Daemon not running:** events wait in the spool (up to about 20,000 hook
  calls). Beyond that, new hook calls are dropped and **counted**; the count
  becomes a recorded `agent_spool_overflow` gap when a daemon next ingests.
- **Malformed or oversized hook input** (> 8 MiB) and **invalid config**:
  recorded as `agent_input_rejected` gaps.
- **Corrupt spool records:** `agent_spool_corrupt` gap.
- **Daemon crash during ingestion:** records are deleted only after their
  events are committed, so a crash replays them; stable event ids make the
  replay harmless.
- Any recorded agent loss that overlaps a session marks it `partial`.

## Cost

Measured on the reference laptop (2018 4-core Intel MacBook, release build):

- A hook call adds about **15 ms** to the agent (p50; p95 ≈ 23 ms), almost
  all of it starting the `tailflow` process. TailFlow's own work is about
  0.6 ms. Claude Code runs it twice per tool call (before and after).
- The daemon ingests a hook call within about **0.2 s**.
- Reproduce with `python3 scripts/bench/agent_bench.py --bin target/release`.

## Uninstall

```bash
tailflow agent uninstall claude
tailflow agent uninstall codex
```

This removes TailFlow's hooks and leaves everything else in the file
unchanged. History stays until retention removes it or you delete it:

```bash
tailflow-daemon storage purge --session <id> --yes   # one session
tailflow-daemon storage purge --all --yes            # everything
```

The spool and per-user agent config live in `<data dir>/agents/`; delete that
directory to remove them.

## Troubleshooting

| Symptom | Check |
|---|---|
| `tailflow sessions` shows nothing | `tailflow agent status`: are the hooks installed in the settings file your agent actually uses? Did you start a **new** session after installing? |
| Claude Code: no events in an interactive session | Interactive sessions run hooks only after the folder's workspace-trust dialog is accepted. Managed policy (`allowManagedHooksOnly`, `disableAllHooks`) can block user hooks; `status` reports it. |
| Codex: no events | Run `/hooks` in Codex and trust the TailFlow hooks. `[features] hooks = false` in `config.toml` disables all hooks. |
| Events stay "waiting" in the spool | No daemon is running. Run `tailflow sessions`, `tailflow-daemon agent ingest`, or `tailflow-daemon --agents`. Check that `TAILFLOW_DATA_DIR` is the same for the agent and the daemon. |
| Debugging the hook | `TAILFLOW_HOOK_DEBUG=1` makes the hook print a one-line report to stderr. |
| Hooks stopped after upgrading TailFlow | `tailflow agent status` reports a missing hook binary; run `tailflow agent install …` again from the new install. |
| Codex prints shell-profile text into the conversation | Codex runs hooks through your login shell (`$SHELL -lc`); anything your profile prints on startup reaches the hook's output. Make the profile quiet for non-interactive shells. |
