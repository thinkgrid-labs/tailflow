# TailFlow

**Runtime verification for coding agents.**

[![CI](https://github.com/thinkgrid-labs/tailflow/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/tailflow/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/tailflow?color=cb3837)](https://www.npmjs.com/package/tailflow)
[![Crates.io](https://img.shields.io/crates/v/tailflow-core?color=f74c00)](https://crates.io/crates/tailflow-core)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Coding agents can inspect source code, run tests, and read git history. But a
passing build does not prove that the application started, hot-reloaded, handled
a request, or stayed healthy afterward.

TailFlow turns local runtime output into compact, queryable evidence so an agent
can verify its change against the software that is actually running.

```text
capture the stack → understand the signal → wait for the outcome → verify the change
```

It collects dev processes, Docker containers, and log files once, then exposes
the same bounded view through MCP, a shell CLI, a TUI, and a web dashboard.

An agent can use that evidence to answer:

- Did the service actually start?
- Did my edit trigger a successful rebuild?
- What new failure appeared after my change?
- Is this one error or the same crash repeated 400 times?

## The problem TailFlow solves

The missing part of most agent workflows is not code access. It is **runtime
awareness**.

```text
Without TailFlow
agent edits → code looks correct → you notice a terminal error → you paste it back

With TailFlow
agent edits → waits for runtime output → reads the failure → fixes and verifies
```

## Capabilities, framed by the job they do

| Job | Capability | Outcome |
|---|---|---|
| Capture | Follow processes, Docker containers, and files in one timeline | The agent sees the stack, not one terminal tab |
| Orient | Track configured source lifecycle and activity | “No errors” is distinguishable from “never started” |
| Understand | Detect severity, group repetitions, and retain stack context | A crash loop becomes one explainable failure |
| Synchronize | Wait server-side for matching runtime output | Async software needs no arbitrary sleep-and-poll loop |
| Compare | Continue from a baseline cursor and report history gaps | New evidence can be separated from pre-existing failures |
| Inspect | Share the model through MCP, shell, browser, and TUI | Developers and agents can examine the same runtime facts |

See the [feature guide](docs/features.md) for use cases, interface mapping, and
the boundary of each capability.

TailFlow is deliberately a **local runtime-verification layer**. It is not a
hosted log platform, an APM product, or a replacement for production monitoring.

## Quick start

### 1. Install

```bash
npm install -g tailflow
```

This installs four commands: `tailflow`, `tailflow-daemon`, `tailflow-mcp`, and
`tailflow-logs`.

### 2. Initialize the project

```bash
cd your-project
tailflow init
```

TailFlow detects development scripts, Compose files, and common log files, lets
you choose the sources, and writes `tailflow.toml`. Use `tailflow init --yes` to
accept the recommended sources noninteractively.

Nothing is overwritten unless you pass `--force`. For manual configuration and
explicit `--process`, `--file`, or `--docker` setup, see the
[getting-started guide](docs/getting-started.md#initialize-a-project).

### 3. Start the collector

```bash
tailflow-daemon
```

The dashboard is now available at <http://127.0.0.1:7878>.

Check the connection from another terminal:

```bash
tailflow-logs status
tailflow-logs sources
```

### 4. Connect an agent

For Claude Code:

```bash
claude mcp add tailflow -- tailflow-mcp
```

For another MCP client:

```json
{
  "mcpServers": {
    "tailflow": {
      "command": "tailflow-mcp"
    }
  }
}
```

The MCP client starts `tailflow-mcp`; you keep `tailflow-daemon` running in the
project. The MCP process is stateless and connects to the daemon at
`http://127.0.0.1:7878` by default.

For installation alternatives, configuration recipes, and troubleshooting, see
the [getting-started guide](docs/getting-started.md).

## Close the verification loop

The features come together in one workflow:

```text
1. Check that the expected sources are running
2. Record the latest cursor as a baseline
3. Make the code change
4. Wait for the rebuild, request, job, or failure caused by that change
5. Read distinct errors or exact lines if the result needs investigation
6. Repeat until the runtime evidence matches the intended outcome
```

Four focused MCP tools support that loop:

| Tool | Use it for |
|---|---|
| `list_log_sources` | Confirm which services started, exited, failed, or were observed |
| `get_recent_errors` | Read distinct recent failures with counts and stack-trace context |
| `search_logs` | Inspect exact lines and event ordering with filters and cursors |
| `wait_for_logs` | Wait for a rebuild, error, or other runtime event without polling |

See [TailFlow for agents](docs/agents.md) for arguments, output contracts,
workflow patterns, shell commands, and the HTTP API.

## Human interfaces

The same sources are available without an agent.

### Terminal UI

```bash
tailflow                 # sources from tailflow.toml
tailflow --docker        # add all running containers
npm run dev | tailflow   # inspect one piped process
```

| Key | Action |
|---|---|
| `/` | Filter by substring or regular expression |
| `j` / `k`, `↓` / `↑` | Scroll; scrolling up pauses follow mode |
| `G` | Return to the latest line and resume following |
| `p` | Toggle JSON pretty-printing |
| `q`, `Ctrl-C` | Quit |

### Shell client

```bash
tailflow-logs errors --since 5m
tailflow-logs search 'timeout' --source api
tailflow-logs wait --grep 'compiled successfully|Failed to compile'
tailflow-logs sources
```

Add `--json` for machine-readable output. Exit codes distinguish success, bad
requests, wait timeouts, and an unavailable daemon.

### Web dashboard

`tailflow-daemon` serves an embedded dashboard at
<http://127.0.0.1:7878> with source counts, severity filters, regex search, and
scroll-aware live following.

## Choose the right entry point

| Command | Purpose | Runs for |
|---|---|---|
| `tailflow-daemon` | Collect logs, retain the buffer, serve HTTP/SSE and the dashboard | Your development session |
| `tailflow-mcp` | Bridge one MCP client to the daemon over stdio | The MCP client session |
| `tailflow-logs` | Make one query from a shell or script | One command |
| `tailflow` | View configured sources in an interactive terminal | Your TUI session |

## Installation options

The npm package requires no Rust toolchain and downloads only the binary package
for the current operating system and CPU.

```bash
npm install -g tailflow
# or run without a permanent install
npx tailflow@latest --docker
```

Prebuilt downloads support macOS ARM64/x64, Linux ARM64/x64, and Windows x64.
They are attached to each [GitHub release](https://github.com/thinkgrid-labs/tailflow/releases).

Build from source with Rust 1.88 or newer:

```bash
git clone https://github.com/thinkgrid-labs/tailflow.git
cd tailflow
cargo install --path crates/tailflow-tui
cargo install --path crates/tailflow-daemon
cargo install --path crates/tailflow-agent
```

## Current limitations

The important boundaries are:

- TailFlow listens on loopback and has no authentication or TLS. It is intended
  for one developer machine, not a shared or internet-facing server.
- The log buffer is memory-only and bounded. History is lost when the daemon
  restarts, and very active stacks can evict older records.
- Severity detection and error fingerprinting are heuristics. Unusual log
  formats may be classified or grouped imperfectly.
- Docker support follows all running containers from the local Docker daemon;
  there is no first-class Compose-service selection or Kubernetes source yet.
- TailFlow observes text output. It does not collect traces, metrics, profiles,
  or application state.

Read [limitations and operational boundaries](docs/limitations.md) before using
TailFlow in automation or with sensitive logs.

## Documentation

| Guide | Contents |
|---|---|
| [Documentation index](docs/README.md) | Where to start for each task |
| [Getting started](docs/getting-started.md) | Installation, setup recipes, configuration, and troubleshooting |
| [Features](docs/features.md) | Capabilities organized by the problem and outcome |
| [TailFlow for agents](docs/agents.md) | MCP tools, shell client, workflow patterns, and HTTP API |
| [Architecture](docs/architecture.md) | Components, data flow, invariants, and repository layout |
| [Limitations](docs/limitations.md) | Security, retention, inference, and source boundaries |
| [Roadmap](docs/roadmap.md) | Planned directions and explicit non-goals |
| [Contributing](CONTRIBUTING.md) | Development environment and pull requests |
| [Changelog](CHANGELOG.md) | Version-by-version changes |

## Roadmap

The next priorities are reducing setup friction, making before/after verification
more direct, and improving source selection and correlation. Likely directions
include direct MCP transport from the daemon, optional daemon auto-start,
Compose-aware discovery, cursor-based error diffing, and request correlation.

The roadmap describes direction, not a release commitment. See the full
[roadmap](docs/roadmap.md).

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) and
open an issue before starting a large feature or architectural change.

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

## License

MIT — see [LICENSE](LICENSE). © 2026 ThinkGrid Labs.
