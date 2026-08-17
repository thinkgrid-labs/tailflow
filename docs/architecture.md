# TailFlow architecture

TailFlow separates log collection from presentation. One ingestion engine feeds
human and agent interfaces without coupling those interfaces to individual log
sources.

[Documentation index](README.md)

## System overview

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

### `tailflow-daemon`

Runs configured sources, tracks their lifecycle, retains a bounded in-memory
ring, and serves:

- the embedded Preact dashboard;
- JSON query endpoints;
- an SSE stream for the browser; and
- long-polling for event-driven agent workflows.

The server binds to loopback and validates the Host header. It is a local API,
not a remotely exposed service.

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

## Design invariants

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
rejects nonlocal Host headers. Remote access would require a separate security
model rather than an accidental bind-address change.

## Repository layout

```text
tailflow/
├── crates/
│   ├── tailflow-core/      ingestion, processing, query store
│   ├── tailflow-tui/       interactive terminal UI
│   ├── tailflow-daemon/    HTTP/SSE server and embedded web UI
│   └── tailflow-agent/     MCP and shell clients
├── web/                    Preact dashboard
├── npm/
│   ├── tailflow/           public launcher package
│   └── platforms/          OS/architecture binary packages
├── docs/                   user and design documentation
└── scripts/                release and local packaging helpers
```

## Technology

| Layer | Technology |
|---|---|
| Core language | Rust 2021, Rust 1.88+ |
| Async runtime | Tokio |
| Docker client | Bollard |
| File watching | notify |
| Terminal UI | Ratatui and Crossterm |
| HTTP server | Axum |
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
  receives production assets.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the development workflow.
