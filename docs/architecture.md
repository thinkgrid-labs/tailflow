# TailFlow architecture

TailFlow separates log collection from presentation. One ingestion engine feeds
human and agent interfaces without coupling those interfaces to individual log
sources.

[Documentation index](README.md)

## System overview

```text
Docker / Process / File / Stdin
            │ LogRecord
            ▼
   broadcast bus (4,096) ── Lagged(n) → counted, gap recorded, warn record
            │
            ▼
   daemon fan-out (one task): filter → redact → ring push → SSE broadcast
            │                                      │
            │ try_send (never blocks)              └─► /api/query, /errors, /wait, MCP
            ▼
   bounded queue ── full → counted, gap recorded
            │
            ▼
   writer thread: batch (≤512 / 500 ms) → BEGIN IMMEDIATE … COMMIT (SQLite WAL)
            │                                                        ▲
            └─ retention, heartbeats, gap rows          reader pool ─┘ /api/v1/*
```

The pre-M1 view of the same system:

```text
┌────────────────────────── tailflow-core ──────────────────────────┐
│ Docker ─┐                                                         │
│ Process ┼──► broadcast channel of LogRecord ──┬──► bounded store  │
│ File ───┤                                     │    + grouping     │
│ Stdin ──┘                                     │                  │
└───────────────────────────────────────────────┼──────────────────┘
                                                │
                         ┌──────────────────────┴──────────────────┐
                         │                                         │
                  ┌──────▼──────┐                         ┌────────▼────────┐
                  │ tailflow TUI│                         │ daemon + web UI │
                  └─────────────┘                         │ HTTP API + SSE  │
                                                        └────────┬────────┘
                                                                 │ HTTP
                                                  ┌──────────────┴─────────────┐
                                                  │ tailflow-mcp  tailflow-logs│
                                                  └────────────────────────────┘
```

## Components

### `tailflow-core`

Owns source configuration, ingestion, level detection, filtering, the broadcast
bus, the bounded query store, cursors, and failure fingerprinting. It does not
depend on a UI.

Each source implements the same asynchronous lifecycle and receives a
cancellation token. That lets the TUI and daemon shut down file watchers,
container streams, and spawned process trees consistently.

Its `agent` module holds the coding-agent integrations: adapters that turn
Claude Code and Codex hook payloads into `ObservabilityEvent`s, the owner-only
spool that hands them to a daemon, the hook installer, and read-only Git
discovery. It does not depend on SQLite, so the hook bridge in the `tailflow`
binary stays free of it.

### `tailflow-storage`

The durable event store: one writer thread owning a read-write SQLite
connection, a small pool of read-only connections for queries, transactional
schema migrations, FTS5 full-text search, chunked retention, durable gap rows,
and run tracking for unclean-shutdown detection. It depends on `tailflow-core`
for the event model and privacy policy and is used only by the daemon.

### `tailflow-daemon`

Runs configured sources, tracks their lifecycle, retains a bounded in-memory
ring, and serves:

- the embedded Preact dashboard (served with a strict Content Security
  Policy);
- JSON query endpoints, including read-only aggregates for the dashboard
  (`/api/v1/stats`, `/api/v1/files`);
- an SSE stream for the browser; and
- long-polling for event-driven agent workflows.

The server binds to loopback, validates the Host header, and requires a
credential on every data endpoint: a per-run API token for CLI and MCP
clients, or a browser session created by a one-time sign-in link
([security](security.md)). It is a local API, not a remotely exposed service.

### Dashboard (`web/`)

A Preact single-page app with hash routing and no other runtime dependency.
Pages read the public read-only API; nothing in the browser filters a full
dataset. The live log view keeps the newest 5,000 ring records and renders
only the visible rows (a variable-height virtual list). Agent views poll
durable history while visible. Pure logic (`web/src/lib/`) has Node unit tests
(`npm test`); `web/e2e/` runs Playwright against a real daemon fed through the
real agent hook adapters.

### `tailflow-agent`

Produces two binaries:

- `tailflow-mcp`, a JSON-RPC/MCP stdio server; and
- `tailflow-logs`, a one-shot shell client.

This crate intentionally does not depend on `tailflow-core`. Both binaries are
HTTP clients of an already running daemon, keeping ingestion dependencies and
state out of each agent process.

### `tailflow-tui`

Runs the same core sources directly and renders their broadcast stream with
Ratatui. It does not require the daemon.

### `web`

A Preact/Vite application embedded in the daemon binary at release time. It
hydrates from recent records, subscribes to SSE, deduplicates by sequence number,
and rehydrates after a disconnected stream.

## Data model and flow

Every ingested line becomes a `LogRecord` with:

- an ingestion timestamp;
- a source label;
- a detected severity; and
- the original text payload.

The daemon assigns a monotonic sequence number when storing each record. API
clients use that number as a cursor, avoiding timestamp collisions and repeated
reads. The ring buffer exposes its oldest sequence so a client can detect when a
cursor belongs to an earlier daemon lifetime or has fallen behind retained
history.

Error summaries normalize variable portions of a line—numbers, timestamps,
addresses, UUIDs, and quoted values—to build a bounded fingerprint. The original
sample remains available, while repeated fingerprints collapse into one group
with first/last timestamps and an occurrence count.

## Event model

`ObservabilityEvent` (schema version 1) is the envelope for everything stored:
`event_id`, `timestamp` (source time) and `observed_at` (receive time),
`event_type` from a fixed catalog, `provenance`, `source_type`/`source_id`,
optional `project_id`, `session_id`, `operation_id`, `parent_operation_id`,
`agent_name`, `severity`, `status`, `duration_ms`, `path`, `message`,
`attributes`, `payload`, and `redaction` metadata. Unknown types and unknown
envelope fields are rejected. Runtime lines are converted with
`ObservabilityEvent::from_log_record`; the live path keeps using `LogRecord`.

The persistent cursor is SQLite's `AUTOINCREMENT` `seq`, which never repeats
even after deletes. The writer keeps `observed_at` non-decreasing in `seq`
order and clamps future source timestamps, so a `since` bound translates into
an exact seq bound. See [durable history](storage.md) for the full contract.

## Agent capture path

```text
agent ── hook (stdin JSON) ──▶ tailflow agent hook <agent>
                                 normalize → privacy → spool file (rename)
                                                  │
              tailflow-daemon ingest thread ◀─────┘ (watcher + 30 s rescan)
                 claim by rename → submit with backpressure → flush → delete
                                                  ▼
                                  events + agent_sessions (same transaction)
```

The hook bridge never touches the database or the network, never prints to
stdout and always exits 0. The spool is the only IPC: there is still no HTTP
endpoint that writes. See [agent capture](agent-capture.md).

## Design invariants

### Loss is never silent

Every lossy hop is counted and recorded: source-bus lag, storage-queue
overflow, failed commits, and unclean shutdowns. Persisted history carries gap
rows in the same transaction as the next batch, and responses derive
`completeness` from them.

### Persistence never blocks live logs

Ingestion offers events to storage with `try_send`. A slow disk can cost
durable history (visibly) but cannot stall SSE, the ring, or agents.

### One redaction point

Redaction runs once in the daemon fan-out, before the ring, SSE, legacy
endpoints, MCP, and storage; storage re-applies the stateless rules as defense
in depth.

### Bounded output

The buffer, query limits, payload length, error groups, and attached context all
have explicit bounds. Responses report truncation rather than implying they
contain the entire history.

### Fail loudly for agent callers

Invalid regexes, levels, and durations return explanatory errors. Silently
dropping an invalid filter could turn a bad query into a misleading “no errors”
answer.

### Preserve event sequence

Records are returned chronologically and carry stable sequence numbers for the
current daemon lifetime. SSE hydration and reconnect behavior preserve that
ordering while deduplicating overlaps.

### Graceful source shutdown

Stopping a frontend must stop the underlying source. In particular, spawned
commands use dedicated process groups on Unix and process-tree termination on
Windows so development servers are not left behind.

### Local security boundary

The daemon is intentionally loopback-only, has no permissive CORS layer, and
rejects nonlocal Host headers. Browser requests carrying a non-loopback
`Origin`, and cross-site requests other than top-level navigation, are refused.
Every route except the static UI, `/health` (liveness only without a
credential) and the sign-in link requires the API token or a same-origin
browser session (`auth.rs`, `require_auth` in `routes.rs`). The only POST
endpoints manage credentials (mint a sign-in link and revoke sessions, both
token-only; sign out). No HTTP endpoint mutates or deletes data; purging is
a local CLI command.
Remote access would require a separate security model rather than an
accidental bind-address change.

## Repository layout

```text
tailflow/
├── crates/
│   ├── tailflow-core/      ingestion, processing, query store, agent adapters
│   ├── tailflow-tui/       interactive terminal UI
│   ├── tailflow-storage/   SQLite event store, retention, queries
│   ├── tailflow-daemon/    HTTP/SSE server and embedded web UI
│   └── tailflow-agent/     MCP and shell clients
├── web/                    Preact dashboard (src/lib pure logic, src/pages, e2e/)
├── npm/
│   ├── tailflow/           public launcher package
│   └── platforms/          OS/architecture binary packages
├── docs/                   user and design documentation
└── scripts/                release and local packaging helpers
```

## Technology

| Layer | Technology |
|---|---|
| Core language | Rust 2021, Rust 1.89+ |
| Async runtime | Tokio |
| Docker client | Bollard |
| File watching | notify |
| Terminal UI | Ratatui and Crossterm |
| HTTP server | Axum |
| Durable storage | SQLite 3.54 (bundled via rusqlite 0.41), WAL, FTS5 |
| Web UI | Preact and Vite, embedded with rust-embed |
| Agent protocol | JSON-RPC 2.0 and MCP over stdio |
| Distribution | npm platform packages and GitHub release binaries |

## Adding functionality

- Add ingestion sources under `crates/tailflow-core/src/ingestion/` and preserve
  cancellation behavior.
- Add bounded query operations in the core store before exposing them through
  daemon routes.
- Keep agent rendering separate from HTTP response types so JSON and compact
  text remain available from the same operation.
- Build the web dashboard before compiling a release daemon so `rust-embed`
  receives production assets. Debug builds read `web/dist` from disk, so UI
  changes need only `npm run build`.
- Dashboard data must come from bounded, indexed endpoints. Add an aggregate
  endpoint rather than counting pages of events in the browser.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the development workflow.
