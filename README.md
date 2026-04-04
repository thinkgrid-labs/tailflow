# TailFlow

**The zero-configuration, high-speed local log aggregator for modern full-stack development.**

Stop context-switching between terminal tabs. TailFlow ingests logs from Docker containers, running processes, and log files and multiplexes them into a single, filterable stream — with near-zero overhead.

---

## The Problem

Modern local development stacks are fragmented. A typical session looks like this:

```
Tab 1: docker compose up
Tab 2: npm run dev
Tab 3: go run ./cmd/api
Tab 4: tail -f logs/worker.log
```

When something breaks, you're jumping between four windows trying to correlate a timestamp in one tab with an error in another. The cognitive load compounds with each service you add.

Existing tools solve parts of this:

- **`docker compose logs -f`** — aggregates containers, but nothing else
- **Dozzle** — beautiful Docker log UI, but web-only and Docker-only
- **Logdy** — pipes stdin to a web UI, but one stream at a time
- **mprocs** — runs multiple processes in a TUI, but isn't log-focused
- **lnav** — powerful log file navigator, but no process spawning or Docker

None of them unify all three source types (containers + spawned processes + log files) in a single, filterable, color-coded terminal dashboard. That gap is what TailFlow fills.

---

## What TailFlow Solves

| Problem | TailFlow's Answer |
|---|---|
| Logs scattered across terminal tabs | Single multiplexed TUI dashboard |
| Can't correlate events across services | All sources share one timestamped stream |
| Docker-only or file-only tooling | Docker + processes + files in one tool |
| Heavy agents (Datadog, Elastic) for local dev | Rust binary, < 50 MB RAM |
| Switching tools between project setups | `tailflow.toml` at your repo root |
| No web UI access to local logs | `tailflow-daemon` SSE endpoint at `localhost:7878` |

---

## Features

- **Three ingestion sources:** Docker containers (via socket), spawned processes (`sh -c`), and tailed log files
- **Zero-config startup:** Drop a `tailflow.toml` at your repo root and run `tailflow`
- **Real-time regex filtering:** Press `/` to filter by keyword, source name, or regex pattern
- **Per-source color coding:** Each service gets a distinct color automatically assigned
- **Sub-10ms latency:** Tokio async runtime + broadcast channel; no polling
- **SSE daemon mode:** `tailflow-daemon` exposes an HTTP endpoint for web UI consumption
- **Dual binaries:** `tailflow` (TUI) and `tailflow-daemon` (headless + HTTP) built from the same core

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     tailflow-core                        │
│                                                          │
│  DockerSource ──┐                                        │
│  ProcessSource ─┼──► broadcast::channel<LogRecord> ─┐   │
│  FileSource ────┘                                    │   │
│  StdinSource ───┘                                    │   │
└──────────────────────────────────────────────────────┼───┘
                                                       │
              ┌────────────────────┬───────────────────┘
              │                    │
     ┌────────▼────────┐  ┌────────▼────────────────────┐
     │  tailflow-tui   │  │      tailflow-daemon         │
     │                 │  │                              │
     │  ratatui TUI    │  │  axum HTTP server            │
     │  color-coded    │  │  GET /events     (SSE)       │
     │  regex filter   │  │  GET /api/records (JSON)     │
     │  scroll/search  │  │  GET /health                 │
     └─────────────────┘  └──────────────────────────────┘
```

`tailflow-core` is intentionally dependency-free of any UI framework. The broadcast channel is the only coupling point, which means adding a new presentation layer (web, desktop, etc.) requires touching only the consumer side.

---

## Installation

### From source (requires Rust 1.75+)

```bash
git clone https://github.com/your-org/tailflow
cd tailflow
cargo install --path crates/tailflow-tui
cargo install --path crates/tailflow-daemon
```

### Verify

```bash
tailflow --help
tailflow-daemon --help
```

---

## Usage

### Quick start — Docker

```bash
# Tail all running containers
tailflow --docker

# Tail specific containers + a log file
tailflow --docker --file logs/app.log
```

### Quick start — pipe a process

```bash
# Pipe any process stdout/stderr into the TUI
npm run dev | tailflow
go run ./cmd/api | tailflow
```

### Quick start — config file (recommended for monorepos)

Create `tailflow.toml` at your project root:

```toml
[sources]
docker = true

[[sources.process]]
label = "frontend"
cmd   = "npm run dev --prefix packages/web"

[[sources.process]]
label = "api"
cmd   = "go run ./cmd/api"

[[sources.file]]
path = "logs/worker.log"
```

Then from anywhere inside the repo:

```bash
tailflow
```

TailFlow auto-discovers `tailflow.toml` by walking up from the current directory.

---

## TUI Keybindings

| Key | Action |
|---|---|
| `/` | Enter filter mode |
| `Enter` | Apply filter and return to view |
| `Esc` | Clear filter mode |
| `j` / `↓` | Scroll down one line |
| `k` / `↑` | Scroll up one line |
| `G` | Jump to the most recent log line |
| `q` / `Ctrl-C` | Quit |

### Filtering

The filter bar accepts plain text substrings or full regex patterns. The filter matches against both the log **payload** and the **source name**, so you can narrow to a single service:

```
# Show only logs from the "api" process
api

# Show only error-level lines
error|ERROR|ERR

# Show lines containing a specific request ID
req-[a-f0-9]{8}
```

---

## HTTP Daemon

`tailflow-daemon` runs as a background process and exposes your local log stream over HTTP. This is useful when you prefer a browser-based UI or need to share logs with a teammate on the same network.

```bash
# Start the daemon (auto-discovers tailflow.toml)
tailflow-daemon

# Custom port
tailflow-daemon --port 9000

# Docker only, no config file
tailflow-daemon --docker
```

### Endpoints

| Endpoint | Description |
|---|---|
| `GET /events` | Server-Sent Events stream. One JSON `LogRecord` per event. |
| `GET /api/records` | Last 500 buffered records as a JSON array. |
| `GET /health` | `{"ok": true}` — liveness check. |

### Consuming the SSE stream

```javascript
const source = new EventSource("http://localhost:7878/events");

source.onmessage = (e) => {
  const record = JSON.parse(e.data);
  // { timestamp, source, level, payload }
  console.log(`[${record.source}] ${record.payload}`);
};
```

### LogRecord schema

```json
{
  "timestamp": "2026-04-04T10:23:45.123Z",
  "source":    "api",
  "level":     "error",
  "payload":   "connection refused: postgres:5432"
}
```

`level` is one of: `trace` | `debug` | `info` | `warn` | `error` | `unknown`

---

## Configuration Reference

`tailflow.toml` is optional. When present, TailFlow and TailFlow-Daemon both load it automatically.

```toml
[sources]
# Discover and tail all running Docker containers
docker = false

# Label piped stdin (only active when stdin is not a TTY)
# stdin = "pipe"

# ── File sources ─────────────────────────────────────────
[[sources.file]]
path  = "logs/app.log"
label = "app"           # optional; defaults to the filename

# ── Process sources ───────────────────────────────────────
# TailFlow spawns these and captures stdout + stderr.

[[sources.process]]
label = "frontend"
cmd   = "npm run dev"

[[sources.process]]
label = "api"
cmd   = "go run ./cmd/api"
```

Config values and CLI flags are **additive** — you can always add `--docker` on top of a config file to bring in extra sources.

---

## Project Layout

```
tailflow/
├── tailflow.example.toml          # annotated config reference
├── Cargo.toml                     # workspace
└── crates/
    ├── tailflow-core/             # ingestion engine (no UI deps)
    │   └── src/
    │       ├── lib.rs             # LogRecord, LogLevel, broadcast bus
    │       ├── config.rs          # tailflow.toml parser
    │       └── ingestion/
    │           ├── docker.rs      # bollard: Docker socket integration
    │           ├── file.rs        # notify: filesystem tail
    │           ├── process.rs     # tokio::process: spawn + capture
    │           └── stdin.rs       # async stdin reader
    ├── tailflow-tui/              # `tailflow` binary — ratatui TUI
    └── tailflow-daemon/           # `tailflow-daemon` binary — axum SSE server
```

---

## Roadmap

- [ ] **Phase 3:** `tailflow-web` — Preact web dashboard consuming the daemon SSE endpoint
- [ ] **npm / npx distribution** — ship the binary via napi-rs so `npx tailflow` works without Rust installed
- [ ] **Homebrew formula** — macOS/Linux native install
- [ ] **`--grep` / `--source` daemon flags** — server-side filtering before SSE emission
- [ ] **Process restart policy** — automatically restart a crashed `[[sources.process]]` entry
- [ ] **JSON log pretty-printing** — detect structured JSON payloads and expand them inline

---

## License

MIT
