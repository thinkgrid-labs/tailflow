# Getting started with TailFlow

This guide takes you from installation to a working runtime-verification loop for an
agent. A minimal setup takes about five minutes.

[Documentation index](README.md)

## Before you start

TailFlow works best when:

- your application writes useful text logs to stdout, stderr, Docker, or files;
- you develop on macOS, Linux, or Windows;
- the agent can use MCP or execute shell commands; and
- the daemon and the application run on the same machine.

The prebuilt npm packages support macOS ARM64/x64, Linux ARM64/x64, and Windows
x64. Building from source requires Rust 1.88 or newer.

## Install TailFlow

### npm

```bash
npm install -g tailflow
tailflow --version
```

The package installs:

| Binary | Role |
|---|---|
| `tailflow-daemon` | Collects logs, stores the bounded buffer, and serves clients |
| `tailflow-mcp` | Exposes daemon queries to an MCP client over stdio |
| `tailflow-logs` | Queries the daemon from a shell or script |
| `tailflow` | Displays sources in an interactive terminal UI |

You can try the TUI without installing globally:

```bash
npx tailflow@latest --docker
```

### Prebuilt binaries

Each [GitHub release](https://github.com/thinkgrid-labs/tailflow/releases)
contains archives for macOS and Linux and executable files for Windows. The
Unix archives contain all four commands.

Example for Apple Silicon macOS:

```bash
curl -fsSL https://github.com/thinkgrid-labs/tailflow/releases/latest/download/tailflow-darwin-arm64.tar.gz \
  | tar -xz -C /usr/local/bin
```

Replace `darwin-arm64` with `darwin-x64`, `linux-arm64`, or `linux-x64` as
needed. Choose a destination on your `PATH` that you have permission to write.

### Build from source

```bash
git clone https://github.com/thinkgrid-labs/tailflow.git
cd tailflow
cargo install --path crates/tailflow-tui
cargo install --path crates/tailflow-daemon
cargo install --path crates/tailflow-agent
```

## Configure log sources

Create `tailflow.toml` at the project or monorepo root. TailFlow searches the
current directory and then each parent directory. Relative file paths and
process commands still use the directory where TailFlow was launched, so start
it from the project root unless the configuration uses absolute paths.

Start with one of these recipes, then combine sources as needed.

### Let TailFlow start development processes

```toml
[sources]

[[sources.process]]
label = "web"
cmd = "npm run dev --prefix apps/web"

[[sources.process]]
label = "api"
cmd = "cargo run -p api"
restart = "on-failure"
restart_delay_ms = 1000
```

TailFlow captures stdout and stderr. Stopping TailFlow also stops the spawned
process tree. Restart policies are `never` (the default), `on-failure`, and
`always`; delays double after each restart up to 30 seconds.

### Follow local Docker containers

```toml
[sources]
docker = true
```

TailFlow connects to the local Docker daemon, follows all running containers,
and reconciles the list every two seconds. Containers started later or replaced
by `docker compose up --build` are discovered automatically.

### Follow files written by another process

```toml
[sources]

[[sources.file]]
path = "logs/app.log"
label = "app"

[[sources.file]]
path = "logs/worker.log"
```

The label is optional and defaults to the filename. A file may be absent when
TailFlow starts; it will be followed after it appears. Rotation, replacement,
and truncation are handled.

### Combine sources

```toml
[sources]
docker = true

[[sources.process]]
label = "frontend"
cmd = "npm run dev --prefix apps/web"

[[sources.file]]
path = "logs/jobs.log"
label = "jobs"
```

See [`tailflow.example.toml`](../tailflow.example.toml) for a copyable template.

### Configuration reference

| Location | Field | Required | Meaning |
|---|---|---|---|
| `[sources]` | `docker` | No | Follow all containers on the local Docker daemon; default `false` |
| `[sources]` | `stdin` | No | Capture standard input under this source label |
| `[[sources.file]]` | `path` | Yes | Local file to follow |
| `[[sources.file]]` | `label` | No | Source name; defaults to the filename |
| `[[sources.process]]` | `label` | Yes | Source name for stdout and stderr |
| `[[sources.process]]` | `cmd` | Yes | Command passed to the platform shell |
| `[[sources.process]]` | `restart` | No | `never`, `on-failure`, or `always`; default `never` |
| `[[sources.process]]` | `restart_delay_ms` | No | Initial backoff; default 1,000 ms, doubling to a 30-second cap |

## Start and verify the daemon

Run this from the project root:

```bash
tailflow-daemon
```

You should see the local addresses for the dashboard, SSE stream, and agent API.
In a second terminal, verify the daemon and sources:

```bash
tailflow-logs status
tailflow-logs sources
tailflow-logs errors --since 5m
```

The daemon keeps the newest 5,000 records by default. Change that with
`tailflow-daemon --buffer N`.

### Add sources without a config file

CLI source flags are additive to `tailflow.toml`:

```bash
tailflow-daemon --docker
tailflow-daemon --file logs/app.log --file logs/worker.log
tailflow-daemon --config path/to/tailflow.toml
```

Use `--grep REGEX` or `--source NAME` only when you intentionally want to drop
nonmatching records before they enter the buffer.

## Connect an agent

The daemon collects and retains logs. `tailflow-mcp` is a small, stateless MCP
server that forwards tool calls to it.

### Claude Code

```bash
claude mcp add tailflow -- tailflow-mcp
```

### Other MCP clients

Add this to the client's MCP server configuration:

```json
{
  "mcpServers": {
    "tailflow": {
      "command": "tailflow-mcp"
    }
  }
}
```

If the daemon uses another port:

```json
{
  "mcpServers": {
    "tailflow": {
      "command": "tailflow-mcp",
      "env": {
        "TAILFLOW_URL": "http://127.0.0.1:8787"
      }
    }
  }
}
```

The URL resolution order is the `tailflow-mcp --url` flag, `TAILFLOW_URL`, then
`http://127.0.0.1:7878`.

After connecting, ask the agent to call `list_log_sources`. It should report the
configured services even when they have not printed a log line yet.

See [TailFlow for agents](agents.md) for the four tools and recommended
verification workflows.

## Use TailFlow without MCP

An agent with shell access can use the same capabilities:

```bash
tailflow-logs sources
tailflow-logs errors --since 10m --source api
tailflow-logs search 'request_id=abc' --limit 100
tailflow-logs wait --grep 'compiled|error' --timeout-ms 30000
```

Add `--json` to receive the daemon response directly. The exit codes are:

| Code | Meaning |
|---|---|
| `0` | Request succeeded; for `wait`, a match occurred |
| `1` | Invalid request or another request error |
| `2` | `wait` timed out without a match |
| `3` | No daemon is reachable |

## Human views

Open <http://127.0.0.1:7878> while the daemon is running, or start the TUI:

```bash
tailflow
tailflow --docker
npm run dev | tailflow --stdin web
```

In the TUI, `/` filters, scrolling up pauses live follow, `G` resumes at the
latest line, `p` toggles JSON formatting, and `q` exits.

## Troubleshooting

### `no sources`

TailFlow could not find a `tailflow.toml`, and no source flags or piped input
were provided. Run it below the correct project root, pass `--config`, or add a
source such as `--docker` or `--file`.

### The MCP tool says no daemon is listening

Start `tailflow-daemon` in the project. If it uses a nondefault port, set the
same `TAILFLOW_URL` in the MCP client configuration. Confirm with:

```bash
tailflow-logs --url http://127.0.0.1:7878 status
```

### Docker discovery fails

Confirm Docker is running and that the current account can access its local
socket. TailFlow reports discovery errors as source records and retries instead
of permanently giving up.

### A configured process exits immediately

Run its `cmd` directly from the project root first. TailFlow uses the platform's
native shell syntax and inherits its own working directory and environment. Use
`restart = "on-failure"` only after the command itself is correct.

### No errors are reported, but the stack is not healthy

Call `tailflow-logs sources` before relying on an empty error list. A source can
be quiet, starting, exited, or failed without producing an application-level
error line.

### Expected older logs are missing

The buffer is memory-only and bounded. Increase `--buffer` for a longer horizon.
History is not retained across daemon restarts. Cursor-aware responses explicitly
report when an incremental request has fallen behind the buffer.

## Next steps

- Understand the capability model in [Features](features.md).
- Learn reliable agent workflows in [TailFlow for agents](agents.md).
- Review the [limitations](limitations.md) before automating decisions.
- Read the [architecture](architecture.md) if you want to contribute.
