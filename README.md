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

None of them unify all three source types (containers + spawned processes + log files) in a single, filterable, color-coded view — in both a TUI and a web UI. That gap is what TailFlow fills.

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
- **Real-time regex filtering:** Filter by keyword, source name, or regex pattern — in both TUI and web UI
- **Per-source color coding:** Each service gets a distinct color; palette is consistent between TUI and web
- **Sub-10ms latency:** Tokio async runtime + broadcast channel; no polling
- **Dual binaries:** `tailflow` (TUI) and `tailflow-daemon` (headless HTTP + embedded web UI)
- **Embedded web dashboard:** `tailflow-daemon` serves a full Preact dashboard at `localhost:7878` — no separate install

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
     ┌────────▼────────┐  ┌────────▼───────────────────────────┐
     │  tailflow-tui   │  │        tailflow-daemon              │
     │                 │  │                                     │
     │  ratatui TUI    │  │  axum HTTP server                   │
     │  color-coded    │  │  GET /events      (SSE stream)      │
     │  regex filter   │  │  GET /api/records (last 500 JSON)   │
     │  scroll/search  │  │  GET /health                        │
     └─────────────────┘  │  GET /*           (embedded web UI) │
                          └─────────────────────────────────────┘
                                       │
                          ┌────────────▼────────────┐
                          │     tailflow-web         │
                          │  (Preact, embedded in    │
                          │   the daemon binary)     │
                          │                          │
                          │  ● source sidebar        │
                          │  ● level filter pills    │
                          │  ● regex search bar      │
                          │  ● auto-scroll + pause   │
                          └──────────────────────────┘
```

`tailflow-core` is intentionally dependency-free of any UI framework. The broadcast channel is the only coupling point, which means adding a new presentation layer (web, desktop, etc.) requires touching only the consumer side.

---

## Installation

### npm / npx (no Rust required)

```bash
# Run once without installing
npx tailflow --docker
npx tailflow-daemon --docker

# Install globally
npm install -g tailflow
tailflow --docker
tailflow-daemon --port 7878
```

npm automatically downloads only the binary for your OS and CPU — no Rust toolchain needed. Supported platforms: macOS ARM64, macOS x64, Linux x64, Linux ARM64, Windows x64.

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

## Web Dashboard

`tailflow-daemon` embeds a full Preact web dashboard into the binary. Once the daemon is running, open your browser — no separate server or `npm install` needed.

```
http://localhost:7878
```

![TailFlow Web UI](docs/screenshot.png)

### Dashboard features

| Feature | Detail |
|---|---|
| **Source sidebar** | All active sources listed with color dots and record counts. Click to filter to a single source; click again to deselect. |
| **Level pills** | `ERR` `WRN` `INF` `DBG` `TRC` pills in the header. Toggle individual levels on/off. |
| **Filter bar** | Plain text substring or full regex. Matches against both the log payload and the source name. |
| **Auto-scroll** | Follows new records automatically. Scrolling up pauses; a **↓ latest** button appears to resume. |
| **Color consistency** | Source colors match the TUI palette exactly — the same source is always the same color. |
| **60fps rendering** | Incoming records are batched to `requestAnimationFrame` cadence so a high-velocity stream doesn't thrash the browser. |

### Building the web UI

The web UI is built with Vite + Preact and the output is embedded into the daemon binary at compile time via `rust-embed`. You must build it before running `cargo build`:

```bash
cd web
npm install
npm run build    # outputs to web/dist/
cd ..
cargo build -p tailflow-daemon --release
```

For web UI development with hot reload:

```bash
# Terminal 1 — run the daemon (sources active)
cargo run -p tailflow-daemon -- --docker

# Terminal 2 — Vite dev server with proxy to daemon
cd web && npm run dev
# open http://localhost:5173
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
├── web/                           # Preact web dashboard
│   ├── package.json
│   ├── vite.config.ts             # dev proxy → daemon :7878
│   └── src/
│       ├── App.tsx                # layout, filter state, auto-scroll
│       ├── types.ts               # LogRecord, color palette
│       ├── hooks/
│       │   └── useLogStream.ts    # EventSource + RAF batching
│       └── components/
│           ├── LogRow.tsx         # single log line
│           └── Sidebar.tsx        # source list with counts
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
    └── tailflow-daemon/           # `tailflow-daemon` binary — axum + embedded web UI
npm/
├── tailflow/                      # published as `tailflow` on npm
│   ├── package.json               # optionalDependencies → platform packages
│   └── bin/
│       ├── run.js                 # shared launcher (platform detection + spawnSync)
│       ├── tailflow.js            # `npx tailflow`
│       └── tailflow-daemon.js     # `npx tailflow-daemon`
└── platforms/                     # published as `@tailflow/<platform>`
    ├── darwin-arm64/package.json
    ├── darwin-x64/package.json
    ├── linux-x64/package.json
    ├── linux-arm64/package.json
    └── win32-x64/package.json     # bin/ directories are gitignored; added by CI
scripts/
├── bump-version.js                # sync version across all package.json + Cargo.toml
└── pack-local.sh                  # build + pack for the current platform (local testing)
.github/workflows/
└── release.yml                    # tag → build all platforms → publish to npm
```

---

## Roadmap

- [x] **Phase 1:** Rust core, ratatui TUI, Docker/file/stdin ingestion
- [x] **Phase 2:** Process spawning, `tailflow.toml` config, axum SSE daemon
- [x] **Phase 3:** Preact web dashboard embedded in the daemon binary
- [x] **npm / npx distribution** — platform-specific optional deps; `npx tailflow` works with no Rust installed
- [ ] **Homebrew formula** — macOS/Linux native install
- [ ] **`--grep` / `--source` daemon flags** — server-side filtering before SSE emission
- [ ] **Process restart policy** — automatically restart a crashed `[[sources.process]]` entry
- [ ] **JSON log pretty-printing** — detect structured JSON payloads and expand them inline

---

## License

MIT
