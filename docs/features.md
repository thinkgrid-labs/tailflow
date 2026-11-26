# TailFlow features

TailFlow is not primarily a log viewer. Its product job is to help a coding
agent **verify a change against a running local stack**.

[Documentation index](README.md)

## The feature model

The capabilities follow the verification loop rather than the shape of the
implementation:

| Job | TailFlow capability | Evidence produced |
|---|---|---|
| Observe | Capture the local stack | One ordered stream from configured runtime sources |
| Orient | Track source lifecycle | Which expected sources started, exited, or failed |
| Understand | Condense failures | Distinct errors, counts, severity, and trailing context |
| Synchronize | Wait for an event | The matching line and the output burst that follows it |
| Compare | Continue from a cursor | Only records newer than the chosen baseline |
| Inspect | Share several interfaces | MCP, shell, browser, and TUI views over the same model |

## 1. Capture the local stack

### Problem

A multi-service application usually spreads runtime output across terminal tabs,
container streams, and files. An agent may be able to launch a command, but it
cannot continuously watch every existing source while it works.

### Capability

TailFlow collects:

- stdout and stderr from processes it starts;
- logs from containers on the local Docker daemon;
- appended lines from local files; and
- piped standard input.

The sources share timestamps, labels, detected levels, and one ordered record
model. CLI source flags can add temporary inputs on top of `tailflow.toml`.

`tailflow init` reduces the initial configuration work by detecting package
development scripts, Compose files, and common log files. It proposes sources
and writes the configuration only after an interactive or explicit selection.

### Runtime behavior

- Docker discovery is continuous. New and replacement containers are attached
  without restarting TailFlow.
- File sources can begin before the file exists and survive replacement,
  rotation, and truncation.
- Process sources capture both output streams, support bounded restart backoff,
  and terminate their process tree when TailFlow stops.

### What this proves

It proves that TailFlow captured the output visible through the configured local
sources during the retained window. It does not prove that every application
event produced a log line.

## 2. Know what is actually running

### Problem

An empty error query is ambiguous. The service may be healthy and quiet, still
starting, already exited, or never connected.

### Capability

The daemon maintains a source registry alongside the log ring:

- `starting` — configured but not yet entered its source task;
- `running` — the source task is active;
- `exited` — the source task ended without reporting an error;
- `failed` — the source task returned an error; and
- `observed` — records exist for a dynamically discovered source whose current
  lifecycle cannot be proven from the bounded ring alone.

Source summaries also carry record, error and warning counts, last activity, and
the latest line.

### What this proves

Lifecycle state describes TailFlow's source task. For a configured process,
`running` means the process supervisor is active; it is not an application-level
readiness or health check. Use a known readiness line or request result when
readiness matters.

## 3. Turn noisy logs into failure evidence

### Problem

Raw logs are expensive agent context. One crash loop can repeat the same failure
hundreds of times, while the useful stack frames appear as unclassified lines
after the error.

### Capability

TailFlow provides:

- severity inference from common text markers and structured JSON level fields;
- token-aware matching that avoids common false positives such as `0 errors`;
- normalized fingerprints that replace changing IDs, numbers, timestamps,
  addresses, and quoted values;
- one group per distinct fingerprint with occurrence count and first/last time;
- trailing same-source context for stack frames and continuation lines; and
- explicit group, record, context, and payload limits.

Example:

```text
Instead of
  connection refused: postgres:5432 (attempt 1)
  connection refused: postgres:5432 (attempt 2)
  ...398 more lines...

The agent gets
  x400 connection refused: postgres:5432 (attempt 400)
       at Pool.connect (...)
```

### What this proves

Grouping is intentionally lossy. It proves that similar retained lines occurred,
not that every grouped payload was semantically identical. Use exact log search
when changing values or ordering matter.

## 4. React to events instead of polling

### Problem

Hot reloads, background jobs, container rebuilds, and integration requests are
asynchronous. Fixed sleep commands are either too short to be reliable or longer
than necessary.

### Capability

`wait_for_logs` blocks in the daemon until a filter matches or a deadline is
reached. When a line matches, TailFlow briefly collects the following burst and
returns it unfiltered from the trigger onward, preserving related stack frames
and downstream output.

A wait can match:

- a successful compilation message;
- an error at or above a chosen severity;
- output from one source;
- a request, job, or test identifier; or
- any regular expression meaningful to the application.

### What this proves

A match proves that a matching retained or new line appeared. A timeout proves
only that no line matched before the deadline. Pass a baseline cursor when an old
matching line must not satisfy a new wait.

## 5. Verify the effect of one change

### Problem

“There are errors in the logs” does not answer the question an agent usually
cares about: “Did my change introduce an error?”

### Capability

Each stored record receives a monotonic sequence number. Query responses return
a `next_cursor`; passing that value into the next search or wait excludes all
earlier records.

```text
baseline cursor 241
        │
        ├── agent edits code
        ├── hot reload begins
        └── wait(cursor: 241) returns only the resulting event
```

The daemon also reports its oldest retained cursor and whether a requested
cursor has fallen behind the ring or belongs to an earlier daemon lifetime.

### What this proves

A complete cursor window lets the caller attribute captured records to the time
after its baseline. A cursor gap means the evidence is incomplete and must not
be presented as an exact before/after comparison.

## 6. Give agents and developers the same evidence

TailFlow offers several interfaces because the workflow changes, while the
underlying evidence should not.

| Interface | Best for |
|---|---|
| MCP tools | Agent reasoning and bounded verification loops |
| `tailflow-logs` | Shell-only agents, scripts, and quick one-shot checks |
| Web dashboard | Investigating agent sessions, searching history, comparing agents on a timeline, following live logs ([guide](dashboard.md)) |
| `tailflow` TUI | Terminal-first live inspection without running the daemon |
| HTTP API and SSE | Custom local clients and integrations |

### Capability matrix

| Capability | MCP | Shell | Dashboard | TUI |
|---|---:|---:|---:|---:|
| Source lifecycle and counts | Yes | Yes | Yes | Logs only |
| Deduplicated error groups | Yes | Yes | No | No |
| Agent sessions, activity search, timeline | API | Sessions only | Yes (next release) | No |
| Exact filtered records | Yes | Yes | Yes | Yes |
| Server-side event waiting | Yes | Yes | No | Live stream |
| Cursor-based incremental reads | Yes | Yes | Internal | No |
| Machine-readable JSON | Yes | Yes | API | No |

## End-to-end use cases

### Verify a hot-reload edit

1. Confirm the frontend source is active.
2. Record the current cursor.
3. Edit the file.
4. Wait after the cursor for `compiled|error|failed`.
5. If the rebuild failed, inspect grouped errors and exact source lines.

### Diagnose a crash loop

1. List sources to find the failed or noisy service.
2. Read distinct errors instead of the raw repeated stream.
3. Search that source around the latest occurrence when exact ordering matters.
4. Apply a fix and establish a new cursor before the restart.

### Follow an asynchronous job

1. Record the cursor before triggering the job.
2. Wait for its job or correlation identifier.
3. Continue waiting from each returned cursor until success or failure appears.

### Survive a Docker rebuild

1. Keep `docker = true` enabled in the daemon.
2. Rebuild or replace the container normally.
3. TailFlow reconciles the new container ID and begins following its output.
4. Verify the new startup event rather than assuming replacement implies health.

## 7. Keep history that survives restarts

### Problem

An agent that restarts the daemon, or comes back the next morning, loses every
log line held only in memory — and cannot tell a quiet night from a crash.

### Capability

- Local SQLite history of runtime logs and collector lifecycle, 30 days and
  2 GB by default, queryable through `/api/v1/events` by type, source, severity,
  time, path, and full text.
- Persistent cursors that keep increasing across restarts.
- Redaction of common credential formats before display or storage, with
  `metadata`, `redacted`, and `full` capture modes.
- A `completeness` verdict on every history response, driven by recorded gaps
  (bus lag, queue overflow, write failures, unclean shutdowns, deletions).
- Storage and collector health in `/api/v1/storage/status`,
  `/api/v1/collectors/status`, and the dashboard diagnostics panel.

### What this proves

That an event was received and committed — or, when `complete` is `false`,
exactly that the record is known to be incomplete. History does not prove what
happened while no daemon was running. See [durable history](storage.md).

## Agent capture (next release)

**What it does.** `tailflow agent install claude|codex` adds observe-only hooks
to the agent's configuration. Each hook call becomes normalized events
(`agent.session.*`, `agent.tool.*`, `command.*`, `file.*`, subagents, turns),
grouped into sessions with lifecycle status and coverage. Inspect them with
`tailflow sessions` / `tailflow session <id>` or `GET /api/v1/sessions`.

**What it proves.** That the agent's hook reported the activity. A tool
failure with an exit code, a file the Write tool created, a subagent that ran.
It does not prove that a file changed on disk, and it cannot see what the
agent did through the shell beyond the command line itself. Every session says
whether its history is `complete`, `partial` or `unknown`, and why.

## Verified changes and Git evidence (next release)

**What it does.** For the project directories you list in `[observe]` (or with
`--observe`), TailFlow watches the filesystem and takes bounded, read-only
Git snapshots. Each agent-reported file change is then compared with what was
observed: reported + observed, reported but not seen, unverified, observed
but unreported, or ambiguous. A session's Git changes are split from
changes that existed before it began. See [evidence](evidence.md).

**What it proves.** That a path changed on disk at a time, and how the
worktree differed from `HEAD` at two moments. It does not prove who made a
change: a file watcher cannot see the writing process, and Git cannot tell
an agent's edit from a person's. A match between a report and an
observation is an association, never authorship.

## Live activity, outcomes and related evidence (next release)

**What it does.** The dashboard follows agent activity live through one
authenticated stream of events already committed to history (it resumes
after a disconnect and reports gaps instead of skipping them). Commands are
classified as build, test, lint, typecheck, format or dependency work, with
the outcome the agent reported. For any event, **Related evidence** lists
other events in five tiers: shared operation ids, an independent
observation of the same file, the same session, closeness in time, and one
labelled heuristic.

**What it proves.** Only what each tier says: a recorded link, an
observation, shared context, or nearness in time. TailFlow does not observe
processes, so a command's outcome is what the agent reported. No tier claims
that one event caused another.

## Local access control (next release)

The daemon's API holds agent sessions, paths and commands, so it requires a
credential: a per-run token for the CLI, MCP and scripts, and a one-time
sign-in link for the browser. Pages on other localhost ports cannot read it.
See [security](security.md).

## Feature boundaries

TailFlow observes text emitted by local software. It does not collect metrics,
traces, profiles, internal application state, or production history.
Coding-agent activity is recorded only through the opt-in Claude Code and Codex
hooks, and only as far as those hooks report it (see
[agent capture](agent-capture.md)).
Classification, grouping, and redaction are heuristics, and all retained
evidence is bounded.

Read [limitations and operational boundaries](limitations.md) for the complete
security, retention, platform, and inference model.
