# TailFlow roadmap

TailFlow's direction is to make runtime verification a normal, low-friction step
in an agent coding loop. This roadmap communicates priorities; it is not a
promise of dates or release numbers.

[Documentation index](README.md)

## Product principles

Roadmap work should preserve four properties:

1. **Local first.** A developer should get value without creating an account or
   sending logs to a hosted service.
2. **Bounded for agents.** Every query must protect the model's context window
   and make truncation visible.
3. **Evidence over inference.** TailFlow should report what ran and what printed,
   while clearly distinguishing missing or incomplete evidence.
4. **One collection path.** Human and agent interfaces should share ingestion
   and runtime state rather than implementing separate collectors.

## Next priorities

### Reduce setup friction

- [x] Add `tailflow init` with project detection, interactive selection, safe
  config generation, explicit source flags, and a noninteractive `--yes` mode.
- [ ] Optionally let `tailflow-mcp` start a local daemon when `tailflow.toml` exists
  and no daemon is reachable.
- [ ] Serve MCP directly from the daemon over streamable HTTP, removing the need
  for a separate stdio bridge where clients support remote MCP transports.

### Improve change verification

- Compare recent error groups against a baseline cursor: “what is failing now
  that was not failing before this edit?”
- Return clearer startup/readiness signals for configured sources.
- Add reusable workflow examples for common frameworks and monorepos.

### Improve source selection

- Understand Docker Compose projects and services instead of treating the local
  Docker daemon as one undifferentiated source pool.
- Add include/exclude filters for container discovery.
- Add log-level toggles to the TUI to match the dashboard.

## Later opportunities

### Correlation

- Group records sharing a request, trace, job, or correlation identifier.
- Let an agent follow one operation across several services without broad text
  searches.

### Durable local history

- Offer an optional SQLite-backed bounded store so a short history can survive
  daemon restarts.
- Preserve explicit horizons and retention limits even when persistence is
  enabled.

### Additional interfaces

- Expose sources as MCP resources as well as tools.
- Add a local webhook source for development log drains.
- Support side-by-side source panes in the TUI.

### Runtime-aware fingerprints

- Improve normalization for Rust panics, Java exceptions, Go panics, JavaScript
  stack traces, and other common runtime formats.
- Allow conservative project-level fingerprint rules without turning the daemon
  into a general log-processing language.

## Exploratory ideas

These may be useful but need stronger evidence before becoming commitments:

- pluggable local sources such as Kafka or Redis pub/sub;
- opt-in adapters for hosted development environments;
- lightweight health checks associated with configured processes; and
- richer agent summaries that preserve exact supporting records.

## Explicit non-goals

The following are outside TailFlow's current product framing:

- becoming a hosted, multi-tenant observability platform;
- replacing production log storage, APM, or distributed tracing;
- executing autonomous remediation against services;
- becoming a general-purpose process orchestrator; and
- hiding missing history or uncertain classification behind a “healthy” verdict.

## How roadmap decisions are made

Good feature requests describe the failed workflow first: what the developer or
agent tried, what evidence was unavailable, and why the current tools were not
enough. Open an issue before a large implementation so the scope can be tested
against the principles above.
