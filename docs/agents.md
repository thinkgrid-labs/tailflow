# TailFlow for Agents

Reference for the MCP server, the shell client, and the HTTP API underneath both.

- [Setup](#setup)
- [MCP tools](#mcp-tools)
- [Workflow patterns](#workflow-patterns)
- [Shell client](#shell-client)
- [HTTP API](#http-api)
- [Design notes](#design-notes)

---

## Setup

Two processes, with distinct jobs:

| Process | Job | Lifetime |
|---|---|---|
| `tailflow-daemon` | Runs your stack, collects logs, holds the buffer | You start it; runs as long as you're working |
| `tailflow-mcp` | Serves the buffer to an agent over MCP stdio | Started and stopped by the MCP client |

`tailflow-mcp` is stateless. It can be registered before the daemon exists, and
survives daemon restarts — each tool call is an independent HTTP request.

### Register with Claude Code

```bash
claude mcp add tailflow -- tailflow-mcp
```

### Register with any MCP client

```json
{
  "mcpServers": {
    "tailflow": {
      "command": "tailflow-mcp",
      "env": { "TAILFLOW_URL": "http://127.0.0.1:7878" }
    }
  }
}
```

The daemon address is resolved in this order: `--url` flag, `$TAILFLOW_URL`,
then `http://127.0.0.1:7878`.

### When the daemon isn't running

Tool calls return `isError: true` with:

```
No TailFlow daemon is listening at http://127.0.0.1:7878.
Start one from your project root (it reads tailflow.toml):

    tailflow-daemon

Or point at a different port with TAILFLOW_URL=http://127.0.0.1:PORT.
```

This is reported as a *tool* error rather than a JSON-RPC protocol error,
specifically so the model sees the text and can act on it — a protocol error is
consumed by the MCP client and never reaches the model.

---

## MCP tools

Every tool accepts `format: "json"` to return the raw daemon response instead of
the compact text rendering.

### `list_log_sources`

No arguments. Returns each source with record, error and warning counts, its last
activity time, and its most recent line.

Call it first in a session. It is the only tool that distinguishes **"the service
is quiet"** from **"the service never started"** — both produce an empty error
list, but only one of them means the code is fine.

### `get_recent_errors`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `since` | string | whole buffer | `30s`, `5m`, `2h`, `1d`, or RFC 3339 |
| `source` | string | all | Substring match, so `api` matches `reko-api` |
| `level` | string | `error` | Minimum severity |
| `grep` | string | — | Additional regex the line must match |
| `limit` | int | 20 | Maximum distinct groups |
| `context_lines` | int | 8 | Trailing stack-trace lines per group (0 to omit) |

Returns distinct failures, most recently seen first, each with an occurrence
count and the stack trace that followed its latest occurrence.

Prefer this over `search_logs` for "did anything break". A crash loop produces
hundreds of near-identical lines; this returns one entry with `x400` on it.

### `search_logs`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `grep` | string | — | Regex matched against the line body, not the source name |
| `source` | string | all | Substring match |
| `level` | string | all | Minimum severity |
| `since` | string | whole buffer | As above |
| `limit` | int | 100 (max 1000) | Newest are kept when truncating |
| `cursor` | int | — | Only lines newer than this sequence number |

Returns individual lines in chronological order. Use when sequence matters:
startup ordering, a request's lifecycle, what a process printed before it died.

### `wait_for_logs`

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `grep` | string | — | Regex the line must match |
| `source` | string | all | Substring match |
| `level` | string | all | Only wake at or above this severity |
| `timeout_ms` | int | 30000 (max 120000) | Give up after this long |
| `cursor` | int | — | Ignore anything at or before this sequence number |
| `limit` | int | 100 | Maximum lines returned |

Blocks server-side until a match arrives, then waits 250 ms more to collect the
rest of the burst — a failure is rarely one line — and returns everything at once.

If a match already exists in the buffer, it returns immediately with
`waited_ms: 0`. **Pass a `cursor`** when you only care about events caused by an
action you just took; otherwise a matching line from five minutes ago satisfies
the wait instantly.

---

## Workflow patterns

### Verify a change actually works

```
1. search_logs(limit: 1)                  → note next_cursor
2. …edit the file, triggering hot reload…
3. wait_for_logs(cursor: <that cursor>,
                 grep: "compiled|error|Error",
                 timeout_ms: 30000)
4. get_recent_errors(since: "2m")         → only if step 3 looked bad
```

Taking the cursor *before* the edit is what scopes steps 3 and 4 to your own
change rather than to whatever the stack was already doing.

### Triage a stack that "isn't working"

```
1. list_log_sources                       → is everything actually up?
2. get_recent_errors(since: "10m")        → distinct failures, newest first
3. search_logs(source: "<the failing one>", limit: 50)
                                          → the sequence around the failure
```

### Follow a long-running operation

```
loop:
  wait_for_logs(cursor: <last>, timeout_ms: 60000)
  → returns as soon as anything appears; the response's next_cursor
    becomes the next call's cursor
```

Costs a few tokens per iteration rather than re-reading the buffer each time.

---

## Shell client

`tailflow-logs` exposes the same operations for agents with only a terminal.

```bash
tailflow-logs errors [--since 5m] [--source api] [--level warn]
                     [--grep REGEX] [--limit 20] [--context 8]
tailflow-logs search [REGEX] [--source api] [--level error]
                     [--since 5m] [--limit 100] [--cursor 4210]
tailflow-logs sources
tailflow-logs wait [--grep REGEX] [--source api] [--level error]
                   [--timeout-ms 30000] [--cursor 4210]
tailflow-logs status
```

Global: `--url URL`, `--json`.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success — and for `wait`, the condition occurred |
| 1 | Bad request (invalid regex, level, or duration) |
| 2 | `wait` reached its timeout without matching |
| 3 | No daemon is listening |

So a script can gate on a real condition:

```bash
if tailflow-logs wait --grep 'compiled successfully' --timeout-ms 30000; then
  echo "rebuild succeeded"
else
  tailflow-logs errors --since 1m
fi
```

---

## HTTP API

All endpoints are `GET`, return JSON, and are served on `127.0.0.1` only.

### Shared parameters

| Parameter | Applies to | Meaning |
|---|---|---|
| `grep` | all | Regex against the payload |
| `source` | all | Substring against the source name |
| `level` | all | Minimum severity (`trace`…`error`) |
| `since` | `/api/errors`, `/api/query` | `30s`/`5m`/`2h`/`1d` or RFC 3339 |
| `limit` | all | Max records or groups (capped at 1000) |
| `cursor` | all | Only records with `seq >` this |
| `max_payload_chars` | `/api/errors`, `/api/query` | Per-line elision cap (default 2000) |
| `context_lines` | `/api/errors` | Trailing context per group (max 50) |
| `timeout_ms` | `/api/wait` | Long-poll deadline (max 120000) |

An invalid `grep`, `level`, or `since` returns **400** with
`{"error": "..."}`. It is never ignored — see [Design notes](#failing-loudly).

### `GET /api/errors`

```json
{
  "groups": [{
    "fingerprint": "ERROR connection refused: postgres:<N> (attempt <N>)",
    "count": 40,
    "sources": ["api"],
    "level": "error",
    "first_seen": "2026-08-06T14:07:31.123Z",
    "last_seen":  "2026-08-06T14:07:33.891Z",
    "sample": "ERROR connection refused: postgres:5432 (attempt 39)",
    "context": ["    at Pool.connect (/app/db.js:42:11)"]
  }],
  "total_matching": 41,
  "distinct": 2,
  "truncated": false,
  "buffer_starts_at": "2026-08-06T14:07:31.001Z",
  "next_cursor": 125
}
```

`level` defaults to `error` here (and only here).

### `GET /api/query`

```json
{
  "records": [{
    "seq": 122,
    "timestamp": "2026-08-06T14:07:33.891Z",
    "source": "api",
    "level": "error",
    "payload": "ERROR Cannot find module \"@reko/pricing\""
  }],
  "total_matching": 41,
  "truncated": true,
  "next_cursor": 125
}
```

A record elided by `max_payload_chars` gains `payload_truncated_from` with the
original character count.

### `GET /api/sources`

```json
{
  "sources": [{
    "name": "api", "total": 122, "errors": 41, "warns": 0,
    "last_seen": "2026-08-06T14:07:33.891Z",
    "last_line": "ERROR Cannot find module \"@reko/pricing\""
  }],
  "buffered": 125,
  "cursor": 125
}
```

Sorted by error count, then volume.

### `GET /api/wait`

Same shape as `/api/query`, plus `matched` (bool) and `waited_ms`.

### `GET /health`

```json
{ "ok": true, "version": "0.2.0", "buffered": 125, "cursor": 125 }
```

---

## Design notes

### Deduplication by fingerprint

Grouping uses a normalised template of the line, built by replacing the parts
that vary between occurrences:

| Input | Placeholder |
|---|---|
| UUIDs | `<UUID>` |
| Hex ids and `0x…` runs of 8+ digits | `<HEX>` |
| Clock times, ISO timestamps | `<TIME>`, `<TS>` |
| IPs, `host:port`, versions | `<ADDR>` |
| Other numbers | `<N>` |
| Quoted strings | `<S>` |
| Whitespace runs | single space |

So `connection refused: postgres:5432` and `postgres:5433` group together, while
`permission denied: /var/run/docker.sock` stays separate. Quoted strings collapse
deliberately: `Cannot find module "foo"` and `"bar"` are one class of failure,
and the verbatim text of the latest occurrence survives in `sample`.

The key is capped at 300 characters so a pathological line cannot bloat the
group map.

### Levels, and why `unknown` ranks lowest

Level is inferred from the text. A line with no recognisable marker —
`compiled successfully`, or an indented stack frame — is `unknown`, which ranks
*below* `trace`. It therefore never satisfies a `level >= warn` query, so plain
output can't masquerade as a warning.

That would ordinarily drop stack traces, since continuation lines carry no level
marker. They are recovered separately: after an error group's most recent
occurrence, following `unknown`-level, non-empty records **from the same source**
are attached as `context`, stopping at the first record with a real level. Output
interleaved from other services is skipped rather than treated as a terminator.

### Cursors

Every response carries `next_cursor`, a monotonic sequence number. Passing it
back as `cursor` returns exactly what arrived since — no timestamp-collision
double-reads, no gaps. This is what makes polling a live stack cheap.

Cursors are per-daemon and reset when the daemon restarts.

### Failing loudly

Invalid arguments are rejected with 400 rather than being dropped.

A human passing a bad regex to a TUI sees an empty screen and immediately knows
something is wrong. An agent doesn't have that feedback. Silently ignoring a
malformed `grep` would return "0 errors" — indistinguishable from a healthy
stack, and the agent would report success. Every filter argument is therefore
validated, and every failure explains what was expected.

### Bounded by construction

Three independent limits keep a single query from flooding a context window:

- `limit` on records or groups, capped at 1000 server-side
- `max_payload_chars` per line (default 2000), with the elision marked
- deduplication, which is where the real compression comes from

Every response reports `truncated` and `total_matching`, so a caller always knows
whether it saw everything.

### Buffer horizon

The daemon retains `--buffer` records (default 5000) and reports
`buffer_starts_at`. A query for `since: "1h"` against a buffer holding twenty
minutes of a chatty stack cannot see the full hour — the horizon field is what
lets a caller notice that rather than conclude the first forty minutes were quiet.
