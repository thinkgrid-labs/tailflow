# TailFlow — Local Log Aggregator for Full-Stack Developers

[![CI](https://github.com/thinkgrid-labs/tailflow/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/tailflow/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/tailflow?color=cb3837)](https://www.npmjs.com/package/tailflow)
[![Crates.io](https://img.shields.io/crates/v/tailflow-core?color=f74c00)](https://crates.io/crates/tailflow-core)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Zero-configuration, high-speed log aggregator for local full-stack development — built in Rust.**

TailFlow unifies logs from Docker containers, spawned processes, and log files into a single real-time stream. View them in a color-coded terminal UI or a browser dashboard. No Rust toolchain required — install via `npx`.

```bash
npx tailflow --docker
```

---

## Table of Contents

- [The Problem](#the-problem)
- [What TailFlow Solves](#what-tailflow-solves)
- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
- [TUI Keybindings](#tui-keybindings)
- [Web Dashboard](#web-dashboard)
- [HTTP Daemon & SSE API](#http-daemon)
- [Configuration Reference](#configuration-reference)
- [Architecture](#architecture)
- [Project Layout](#project-layout)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

---

## The Problem

Modern local development stacks are fragmented. A typical session looks like this:

```
Tab 1: docker compose up
Tab 2: npm run dev
Tab 3: go run ./cmd/api
Tab 4: tail -f logs/worker.log
```

When something breaks, you're jumping between four windows trying to correlate a timestamp in one tab with an error in another. The cognitive load compounds with every service you add.

Existing tools solve parts of this:

| Tool | Gap |
|---|---|
| `docker compose logs -f` | Docker containers only — no processes or files |
| Dozzle | Docker-only web UI — can't ingest spawned processes |
| Logdy | One stdin stream at a time — no Docker or multi-source |
| mprocs | Multi-process runner — not log-focused, no filtering |
| lnav | Powerful log file viewer — no Docker or process spawning |

None of them unify all three source types — Docker containers, spawned processes, and log files — in a single filterable, color-coded view with both a TUI and a web UI. That gap is what TailFlow fills.

---

## What TailFlow Solves

| Problem | Solution |
|---|---|
| Logs scattered across terminal tabs | Single multiplexed TUI or browser dashboard |
| Hard to correlate events across microservices | All sources share one timestamped stream |
| Docker-only or file-only log tooling | Docker + processes + files in one tool |
| Heavy agents (Datadog, Elastic) for local dev | Rust binary, < 50 MB RAM, no daemon required |
| Per-project tool configuration | `tailflow.toml` at your monorepo root |
| Terminal-only access to local logs | `tailflow-daemon` SSE endpoint at `localhost:7878` |

---

## Features

- **Unified log ingestion** — Docker containers (via socket), spawned child processes (`sh -c`), tailed log files, and piped stdin
- **Zero-config startup** — drop a `tailflow.toml` at your repo root and run `tailflow` from anywhere inside it
- **Real-time regex filtering** — filter by keyword, source name, or regex in both the TUI and web dashboard
- **Color-coded sources** — each service gets a distinct color; palette is consistent between the TUI and web UI
- **Sub-10ms latency** — Tokio async runtime with a broadcast channel; zero polling
- **Embedded web dashboard** — `tailflow-daemon` serves a Preact UI at `localhost:7878`, no separate install
- **npx-ready** — `npx tailflow` works on macOS, Linux, and Windows without installing Rust
- **Dual binaries** — `tailflow` (interactive TUI) and `tailflow-daemon` (headless HTTP + web UI)

---

## Installation

### npm / npx — no Rust required

The fastest way to get started. Works on macOS (ARM64 + x64), Linux (x64 + ARM64), and Windows x64.

```bash
# One-off run — no install needed
npx tailflow --docker
npx tailflow-daemon --docker

# Global install
npm install -g tailflow
tailflow --docker
tailflow-daemon --port 7878
```

npm installs only the binary matching your OS and CPU via platform-specific optional dependencies — the same distribution pattern used by esbuild and Biome.

### Homebrew (macOS / Linux)

```bash
brew install your-org/tap/tailflow
```

### From source — requires Rust 1.75+

```bash
git clone https://github.com/thinkgrid-labs/tailflow.git
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

### Tail all running Docker containers

```bash
tailflow --docker
```

### Tail containers and a log file together

```bash
tailflow --docker --file logs/app.log
```

### Pipe any process into the TUI

```bash
npm run dev | tailflow
go run ./cmd/api | tailflow
python manage.py runserver | tailflow
```

### Config file — recommended for monorepos

Create `tailflow.toml` at your project root to define your full local stack:

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
path  = "logs/worker.log"
label = "worker"
```

Then from anywhere inside the repo:

```bash
tailflow          # TUI mode
tailflow-daemon   # browser mode → open http://localhost:7878
```

TailFlow auto-discovers `tailflow.toml` by walking up from the current directory.

---

## TUI Keybindings

| Key | Action |
|---|---|
| `/` | Enter filter mode |
| `Enter` | Apply filter and return to stream |
| `Esc` | Exit filter mode |
| `j` / `↓` | Scroll down |
| `k` / `↑` | Scroll up |
| `G` | Jump to latest log line |
| `q` / `Ctrl-C` | Quit |

### Filter syntax

The filter bar accepts plain text substrings or full regex patterns. Matches against both the log payload and the source name:

```
# Show only logs from the "api" source
api

# Show error-level lines (case-insensitive)
(?i)error

# Match lines containing a specific request ID
req-[a-f0-9]{8}

# Show output from multiple sources
frontend|api
```

---

## Web Dashboard

`tailflow-daemon` embeds a full Preact web dashboard into its binary. Start the daemon, then open your browser — no extra install or `npm run` needed:

```
http://localhost:7878
```

### Dashboard features

| Feature | Detail |
|---|---|
| **Source sidebar** | Active sources with color dots and record counts. Click to isolate a source. |
| **Level filter pills** | `ERR` `WRN` `INF` `DBG` `TRC` — toggle individual log levels on/off. |
| **Regex filter bar** | Substring or regex, matched against payload and source name. |
| **Auto-scroll** | Follows new records automatically. Scroll up to pause; **↓ latest** button resumes. |
| **Consistent colors** | Source colors match the TUI palette exactly. |
| **60 fps rendering** | Records are batched to `requestAnimationFrame` cadence — handles high-velocity streams without thrashing. |

### Building the web UI from source

The web UI is compiled with Vite + Preact and embedded into the daemon binary via `rust-embed`. Build it before `cargo build`:

```bash
cd web && npm install && npm run build
cd .. && cargo build -p tailflow-daemon --release
```

For hot-reload development:

```bash
# Terminal 1: run the daemon with live sources
cargo run -p tailflow-daemon -- --docker

# Terminal 2: Vite dev server (proxies /events and /api to the daemon)
cd web && npm run dev
# open http://localhost:5173
```

---

## HTTP Daemon

`tailflow-daemon` runs as a lightweight background process and exposes your local log stream over HTTP. Useful for browser-based inspection or sharing logs with a teammate on the same local network.

```bash
tailflow-daemon                  # auto-discovers tailflow.toml
tailflow-daemon --port 9000      # custom port
tailflow-daemon --docker         # Docker only, no config file
```

### Endpoints

| Endpoint | Description |
|---|---|
| `GET /events` | Server-Sent Events stream — one JSON `LogRecord` per event |
| `GET /api/records` | Last 500 buffered records as a JSON array |
| `GET /health` | `{"ok": true}` liveness check |
| `GET /` | Embedded Preact web dashboard |

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

`tailflow.toml` is optional. When present, both `tailflow` and `tailflow-daemon` load it automatically.

```toml
[sources]
# Tail all running Docker containers
docker = false

# Label piped stdin (active only when stdin is not a TTY)
# stdin = "pipe"

# ── File sources ──────────────────────────────────────────
[[sources.file]]
path  = "logs/app.log"
label = "app"           # optional; defaults to the filename

# ── Process sources ───────────────────────────────────────
# TailFlow spawns these commands and captures stdout + stderr.

[[sources.process]]
label = "frontend"
cmd   = "npm run dev"

[[sources.process]]
label = "api"
cmd   = "go run ./cmd/api"
```

CLI flags are **additive** on top of the config file. `tailflow --docker` adds Docker containers to whatever sources are already defined in `tailflow.toml`.

---

## Architecture

TailFlow separates ingestion from presentation through a Tokio broadcast channel. Adding a new UI (desktop app, VS Code extension, etc.) only requires a new consumer — the core engine is untouched.

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
```

### Tech stack

| Layer | Technology |
|---|---|
| Language | Rust (2021 edition) |
| Async runtime | Tokio |
| Docker integration | bollard |
| File watching | notify |
| TUI framework | ratatui + crossterm |
| HTTP server | axum |
| Web UI | Preact + Vite (embedded via rust-embed) |
| npm distribution | Platform-specific optional dependencies |

---

## Project Layout

```
tailflow/
├── tailflow.example.toml          # annotated config reference
├── Cargo.toml                     # Rust workspace
├── web/                           # Preact web dashboard source
│   ├── package.json
│   ├── vite.config.ts             # dev proxy → daemon :7878
│   └── src/
│       ├── App.tsx                # layout, filter state, auto-scroll
│       ├── types.ts               # LogRecord type, color palette
│       ├── hooks/useLogStream.ts  # EventSource + RAF batching
│       └── components/
│           ├── LogRow.tsx
│           └── Sidebar.tsx
├── crates/
│   ├── tailflow-core/             # ingestion engine — no UI dependencies
│   │   └── src/
│   │       ├── lib.rs             # LogRecord, LogLevel, broadcast bus
│   │       ├── config.rs          # tailflow.toml parser
│   │       └── ingestion/
│   │           ├── docker.rs      # bollard: Docker socket
│   │           ├── file.rs        # notify: filesystem tail
│   │           ├── process.rs     # tokio::process: spawn + capture
│   │           └── stdin.rs       # async stdin reader
│   ├── tailflow-tui/              # `tailflow` binary
│   └── tailflow-daemon/           # `tailflow-daemon` binary
├── npm/
│   ├── tailflow/                  # published as `tailflow` on npm
│   │   └── bin/run.js             # platform detection + spawnSync launcher
│   └── platforms/                 # @tailflow/<platform> packages
│       ├── darwin-arm64/
│       ├── darwin-x64/
│       ├── linux-x64/
│       ├── linux-arm64/
│       └── win32-x64/
└── scripts/
    ├── bump-version.js            # sync version across package.json + Cargo.toml
    └── pack-local.sh              # local build + npm pack for testing
```

---

## Roadmap

- [x] Rust core engine with broadcast bus
- [x] ratatui TUI — color-coded sources, regex filter, keyboard scroll
- [x] Docker, process, file, and stdin ingestion sources
- [x] `tailflow.toml` zero-config discovery
- [x] axum SSE daemon with ring buffer
- [x] Preact web dashboard embedded in the daemon binary
- [x] npm / npx distribution — no Rust toolchain required
- [ ] Homebrew formula for macOS and Linux
- [ ] Server-side `--grep` and `--source` filter flags for the daemon
- [ ] Process restart policy for crashed `[[sources.process]]` entries
- [ ] JSON log pretty-printing — detect structured payloads and expand inline

---

## Contributing

Contributions are welcome. Please open an issue before submitting a large PR so we can align on the approach.

```bash
# Run the full quality gate locally before pushing
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

The CI workflow runs `fmt`, `clippy`, `build`, and `test` on every push and pull request targeting `main` or `dev`.

---

## License

MIT — see [LICENSE](LICENSE).
