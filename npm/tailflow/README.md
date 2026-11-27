# TailFlow

**See what your coding agents actually do.**

TailFlow is local-first observability for AI coding agents such as Claude Code
and Codex. It runs as one small daemon on your machine, needs no account, and
sends nothing off it.

- **Runtime evidence.** Output from local development processes, Docker
  containers and log files, in one timeline that an agent can query over MCP
  or a shell command: confirm services started, collapse repeated failures,
  wait for a rebuild instead of polling.
- **Agent activity.** With opt-in hooks, the sessions, tool calls, commands
  and file edits that Claude Code and Codex report, in a local dashboard next
  to your runtime errors.
- **Independent evidence.** For directories you choose, the file changes and
  Git state TailFlow observed itself, compared with what the agent reported.
  An observation shows that a file changed, never who changed it.

```bash
npm install -g tailflow
tailflow init
tailflow-daemon
claude mcp add tailflow -- tailflow-mcp
```

The package installs four commands:

- `tailflow-daemon` collects logs and agent events and serves the dashboard
  and the local API;
- `tailflow-mcp` connects an MCP client to the daemon;
- `tailflow-logs` queries the daemon from a shell or script; and
- `tailflow` sets up projects and agent hooks and opens the terminal UI.

Read the [project overview](https://github.com/thinkgrid-labs/tailflow#readme),
the [feature guide](https://github.com/thinkgrid-labs/tailflow/blob/main/docs/features.md)
and the [limitations](https://github.com/thinkgrid-labs/tailflow/blob/main/docs/limitations.md).

TailFlow is a local development tool. It is not a hosted log platform or a
replacement for production observability.
