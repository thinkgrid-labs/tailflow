# TailFlow — Give Your Coding Agent Eyes on Your Running Stack

[![CI](https://github.com/thinkgrid-labs/tailflow/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/tailflow/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/tailflow?color=cb3837)](https://www.npmjs.com/package/tailflow)
[![Crates.io](https://img.shields.io/crates/v/tailflow-core?color=f74c00)](https://crates.io/crates/tailflow-core)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your AI agent writes code it cannot see running.** TailFlow captures the local
sources you configure — Docker containers, dev servers, background workers and
log files — and serves their output to the agent over MCP, deduplicated and bounded.

```bash
npm install -g tailflow
tailflow-daemon                       # in your project root
claude mcp add tailflow -- tailflow-mcp
```

Now your agent can answer *"did my change break anything?"* by reading what your
services actually printed, instead of guessing from source code.

---

## Table of Contents

- [The Problem](#the-problem)
- [What the Agent Gets](#what-the-agent-gets)
- [Setup](#setup)
- [Shell Access](#shell-access-agents-without-mcp)
- [For Humans: TUI and Web Dashboard](#for-humans-tui-and-web-dashboard)
- [Installation](#installation)
- [Configuration](#configuration)
- [HTTP API](#http-api)
- [Architecture](#architecture)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

---

## The Problem

A coding agent has excellent access to the *static* half of your project — files,
types, tests, git history — and almost none to the *running* half. When it changes
a service, the feedback loop is:

```
agent edits file → "the change looks correct" → you paste an error back
```

The agent can run a build. It cannot watch four dev servers, notice which one
crash-looped, and correlate that with the file it just touched. So the loop runs
through you.

Everything needed to close that loop is already on your machine, scattered across
terminal tabs. The blocker is that log output is the wrong shape for an agent:

| Problem | Consequence |
|---|---|
| Logs live in TTYs the agent can't read | It has no runtime feedback at all |
| A crash loop emits the same error 400 times | 400 lines of near-identical text swamps the context window |
| Errors and their stack traces are separate lines | A `level >= error` filter drops the trace |
| "Nothing failed" and "nothing started" look identical | The agent reports a green build for a stack that never booted |
| Checking for an async failure means sleeping | Either too short to catch it or too slow to be useful |

TailFlow fixes the shape, not just the access.

---

## What the Agent Gets

Four MCP tools, backed by a running daemon.

### `get_recent_errors` — distinct failures, not repeated lines

Identical errors are collapsed into one group with an occurrence count, and each
group carries the stack trace that followed it. A crash loop that printed 120
lines becomes:

```
2 distinct failures across 41 records · buffer reaches back to 14:07:31 · cursor 125

[1] x1 error api first 14:07:33 last 14:07:33
    ERROR Cannot find module "@reko/pricing"

[2] x40 error api first 14:07:31 last 14:07:33
    ERROR connection refused: postgres:5432 (attempt 39)
      at Pool.connect (/app/db.js:42:11)
      at async bootstrap (/app/main.js:9:3)
```

Grouping is by *fingerprint* — the line with its variable parts (numbers, UUIDs,
hex ids, quoted strings, addresses) replaced by placeholders — so `postgres:5432`
and `postgres:5433` are recognised as one failure while genuinely different
errors stay apart.

### `wait_for_logs` — block until something happens

Replaces sleep-and-poll. The agent triggers a rebuild, then asks to be woken the
moment anything matches:

```
wait_for_logs(grep: "compiled successfully|Failed to compile", timeout_ms: 30000)
→ Matched after 2168ms.
  14:12:34 error web  Failed to compile: Type error in src/app/page.tsx:22
```

It returns on the first match plus the burst that follows it, so the whole event
arrives in one round trip — or it reports plainly that nothing matched, which is
information too.

### `list_log_sources` — what is actually running

```
2 sources · 125 records buffered · cursor 125
api    122 records    41 err     0 warn  last 14:07:33  ERROR Cannot find module "@reko/pricing"
web      3 records     0 err     1 warn  last 14:07:33  WARN slow render: 1240ms
```

This is the tool that stops an agent declaring success over a dead stack:
configured sources report `starting`, `running`, `exited`, or `failed`, including
quiet services that have not emitted a line. Docker containers discovered from
the stream are marked `observed`; the Docker supervisor carries the live status.

### `search_logs` — the exact sequence of events

Individual lines when the summary isn't enough — a request's lifecycle, startup
ordering, what a process printed just before it died. Every response ends with a
cursor; passing it back returns only what has arrived since, so following a live
stack costs a few tokens per poll instead of a re-read.

---

## Setup

TailFlow is two pieces: a **daemon** that collects logs, and an **MCP server**
that serves them to your agent.

**1. Describe your stack** in `tailflow.toml` at your project root:

```toml
[sources]
docker = true                      # running containers, including later replacements

[[sources.process]]
label = "web"
cmd   = "npm run dev --prefix apps/web"

[[sources.process]]
label = "api"
cmd   = "go run ./cmd/api"

[[sources.file]]
path  = "logs/worker.log"
```

**2. Start the daemon** — this also starts the processes above, so it replaces
your `npm run dev` tab rather than adding to it:

```bash
tailflow-daemon
```

**3. Register the MCP server** with your agent:

```bash
# Claude Code
claude mcp add tailflow -- tailflow-mcp
```

<details>
<summary>Other MCP clients (JSON config)</summary>

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

`TAILFLOW_URL` is only needed if the daemon runs on a non-default port.
</details>

The MCP server is a thin client — it holds no state and can be started before
the daemon exists. If the daemon is down, tools return an actionable message
saying so rather than an empty result.

See [docs/agents.md](docs/agents.md) for tool arguments, workflow patterns, and
the design rationale behind the output format.

---

## Shell Access (agents without MCP)

Every tool is also a one-shot command, for agents that only have a terminal:

```bash
tailflow-logs errors --since 5m        # deduplicated failures
tailflow-logs search 'timeout' --source api
tailflow-logs sources
tailflow-logs wait --grep 'compiled successfully' --timeout-ms 30000
tailflow-logs status
```

Exit codes are meaningful, so a script can branch without parsing stdout:
`0` ok (and for `wait`, matched), `1` bad request, `2` `wait` timed out,
`3` no daemon running. Add `--json` to any command for the raw response.

---

## For Humans: TUI and Web Dashboard

The same ingestion engine also drives interactive views.

```bash
tailflow                 # color-coded TUI over your whole stack
tailflow --docker        # or just the containers
npm run dev | tailflow   # or one piped process
```

| Key | Action |
|---|---|
| `/` | Filter (substring or regex, matched against payload and source) |
| `j` `k` / `↓` `↑` | Scroll and pause follow mode |
| `G` | Resume following at the latest line |
| `p` | Toggle JSON pretty-printing |
| `q` / `Ctrl-C` | Quit |

`tailflow-daemon` additionally serves a Preact dashboard at
**http://localhost:7878** — source sidebar with per-service counts, level filter
pills, regex filter, and auto-scroll that pauses when you scroll up.

---

## Installation

### npm / npx — no Rust required

```bash
npm install -g tailflow
```

Installs four binaries: `tailflow` (TUI), `tailflow-daemon` (collector + web UI),
`tailflow-mcp` (MCP server), `tailflow-logs` (shell client). Only the binary
matching your OS and CPU is downloaded, via platform-specific optional
dependencies — the same pattern esbuild and Biome use. macOS (ARM64 + x64),
Linux (x64 + ARM64), Windows x64.

### Direct binary download

Every release attaches prebuilt archives for macOS (ARM64, x64) and Linux
(x64, ARM64), plus raw `.exe` files for Windows, to its
[GitHub release](https://github.com/thinkgrid-labs/tailflow/releases). Each
archive contains all four binaries:

```bash
curl -fsSL https://github.com/thinkgrid-labs/tailflow/releases/latest/download/tailflow-darwin-arm64.tar.gz \
  | tar -xz -C /usr/local/bin
```

Substitute `darwin-x64`, `linux-x64`, or `linux-arm64` as appropriate.

### From source — Rust 1.88+

```bash
git clone https://github.com/thinkgrid-labs/tailflow.git
cd tailflow
cargo install --path crates/tailflow-tui
cargo install --path crates/tailflow-daemon
cargo install --path crates/tailflow-agent
```

---

## Configuration

`tailflow.toml` is discovered by walking up from the current directory. CLI flags
are **additive** on top of it — `tailflow --docker` adds containers to whatever
the file already defines.

```toml
[sources]
docker = false           # continuously discover running containers when enabled
# stdin = "pipe"         # label piped stdin (only when stdin is not a TTY)

[[sources.file]]
path  = "logs/app.log"
label = "app"            # optional; defaults to the filename

[[sources.process]]
label = "frontend"
cmd   = "npm run dev"    # spawned by TailFlow; stdout + stderr captured
```

Daemon flags:

| Flag | Default | Purpose |
|---|---|---|
| `--port` | `7878` | HTTP listen port |
| `--buffer` | `5000` | Records retained for retrospective queries |
| `--docker` | off | Add all running containers |
| `--file PATH` | — | Add a log file (repeatable) |
| `--grep REGEX` | — | Drop non-matching records at ingest |
| `--source NAME` | — | Ingest only matching sources |

---

## HTTP API

The MCP server and CLI are both clients of this; you can call it directly.

| Endpoint | Description |
|---|---|
| `GET /api/errors` | Deduplicated failure groups with stack-trace context |
| `GET /api/query` | Individual records, cursor-paginated |
| `GET /api/sources` | Per-source counters and last line |
| `GET /api/wait` | Long-poll until a matching record arrives |
| `GET /events` | SSE stream — one JSON record per event |
| `GET /api/records` | Last N raw records (drives the web dashboard) |
| `GET /health` | Liveness, version, buffer state |
| `GET /` | Embedded web dashboard |

Shared parameters: `grep` (regex), `source` (substring), `level`
(`trace`…`error`), `since` (`30s`/`5m`/`2h`/`1d` or RFC 3339), `limit`, `cursor`.
Invalid arguments return **400 with an explanation** — never a silently empty
result, which a caller that can't see the screen would misread as "all clear".
Agent responses also report `cursor_gap` when a requested cursor predates the
bounded ring buffer or belongs to an earlier daemon lifetime. The HTTP server
binds to loopback and rejects non-loopback Host headers; it is not a remote log API.

Full reference: [docs/agents.md](docs/agents.md).

---

## Architecture

Ingestion is separated from presentation by a Tokio broadcast channel. Adding a
consumer never touches the collection engine.

```
┌─────────────────────────── tailflow-core ───────────────────────────┐
│  DockerSource ─┐                                                     │
│  ProcessSource ┼──► broadcast::channel<LogRecord> ──┬──► LogStore    │
│  FileSource ───┤                                    │    (ring +     │
│  StdinSource ──┘                                    │    fingerprint │
│                                                     │    grouping)   │
└─────────────────────────────────────────────────────┼───────────────┘
                    ┌────────────────────┬────────────┘
                    │                    │
           ┌────────▼────────┐  ┌────────▼──────────────────────────┐
           │  tailflow-tui   │  │        tailflow-daemon             │
           │  ratatui TUI    │  │  axum: /api/errors  /api/query     │
           └─────────────────┘  │        /api/wait    /api/sources   │
                                │        /events      web dashboard  │
                                └────────┬───────────────────────────┘
                                         │ HTTP
                          ┌──────────────┴──────────────┐
                          │      tailflow-agent          │
                          │  tailflow-mcp   (MCP stdio)  │
                          │  tailflow-logs  (shell CLI)  │
                          └──────────────────────────────┘
```

`tailflow-agent` deliberately does **not** depend on `tailflow-core`. Its two
binaries are HTTP clients of a daemon that is already running, so they carry
none of the ingestion dependency tree and stay small.

| Layer | Technology |
|---|---|
| Language | Rust 2021 |
| Async runtime | Tokio |
| Docker | bollard |
| File watching | notify |
| TUI | ratatui + crossterm |
| HTTP server | axum |
| Web UI | Preact + Vite (embedded via rust-embed) |
| MCP | hand-rolled JSON-RPC 2.0 over stdio (no SDK dependency) |

---

## Roadmap

### Near-term

- [ ] **`tailflow-daemon --mcp`** — serve MCP directly from the daemon over
      streamable HTTP, so agents need no second process
- [ ] **Auto-start the daemon** from `tailflow-mcp` when one isn't running and a
      `tailflow.toml` is present
- [ ] **Docker Compose discovery** — read services from `docker-compose.yml`
      instead of listing them by hand
- [ ] **`get_recent_errors` diffing** — "what is failing now that wasn't before
      my change", using a cursor as the baseline

### High-impact

- [ ] **Request correlation** — group records sharing a trace/request id so an
      agent can follow one request across services
- [ ] **MCP resources** — expose each source as a readable resource, not just
      tools
- [ ] **Persistent buffer** — optional SQLite ring so history survives a daemon
      restart
- [ ] **`[[sources.http]]` webhook receiver** — ingest log drains from Vercel,
      Render, Fly.io as named sources
- [ ] **Log level filter toggles in the TUI** — `e`/`w`/`i`/`d`, matching the
      web dashboard's pills

### Speculative

- [ ] **Fingerprint tuning per-runtime** — language-aware normalisation for
      Rust panics, Java stack traces, Go `panic:` blocks
- [ ] **Plugin system for custom sources** — Kafka, Redis pub/sub, CloudWatch
- [ ] **TUI split-pane view** — two sources side by side

---

## Contributing

Contributions are welcome. Please open an issue before a large PR so we can align
on the approach.

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

CI runs `fmt`, `clippy`, `build`, and `test` on every push and PR targeting
`main` or `dev`.

Release notes for every version live in [CHANGELOG.md](CHANGELOG.md). Bump all
version-carrying files together with `node scripts/bump-version.js <semver>`.

---

## License

MIT — see [LICENSE](LICENSE)  — © 2026 ThinkGrid Labs
