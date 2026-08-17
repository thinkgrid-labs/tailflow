# TailFlow

**Runtime verification for coding agents.**

TailFlow turns output from local development processes, Docker containers, and
log files into bounded evidence an agent can use to verify a code change.

It helps an agent:

- confirm that expected services actually started;
- collapse repeated failures while retaining stack-trace context;
- wait for rebuilds and other asynchronous outcomes without polling; and
- compare new runtime output against a cursor captured before its edit.

```bash
npm install -g tailflow
tailflow init
tailflow-daemon
claude mcp add tailflow -- tailflow-mcp
```

The package installs four interfaces over those capabilities:

- `tailflow-daemon` collects logs and serves the agent API and dashboard;
- `tailflow-mcp` connects an MCP client to the daemon;
- `tailflow-logs` queries the daemon from a shell or script; and
- `tailflow` opens the interactive terminal UI.

Read the [quick start and project overview](https://github.com/thinkgrid-labs/tailflow#quick-start)
or browse the [feature guide](https://github.com/thinkgrid-labs/tailflow/blob/main/docs/features.md)
and [complete documentation](https://github.com/thinkgrid-labs/tailflow/tree/main/docs).

TailFlow is a local runtime-verification layer. It is not a hosted log platform
or a replacement for production observability.
