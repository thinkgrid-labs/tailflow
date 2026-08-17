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
| Web dashboard | Browsing sources and following a development session |
| `tailflow` TUI | Terminal-first live inspection without running the daemon |
| HTTP API and SSE | Custom local clients and integrations |

### Capability matrix

| Capability | MCP | Shell | Dashboard | TUI |
|---|---:|---:|---:|---:|
| Source lifecycle and counts | Yes | Yes | Yes | Logs only |
| Deduplicated error groups | Yes | Yes | No | No |
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

## Feature boundaries

TailFlow observes text emitted by local software. It does not collect metrics,
traces, profiles, internal application state, or durable production history.
Classification and grouping are heuristics, and all retained evidence is bounded.

Read [limitations and operational boundaries](limitations.md) for the complete
security, retention, platform, and inference model.
