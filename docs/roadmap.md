# TailFlow roadmap

TailFlow's direction is to show what coding agents actually do, and whether
their changes work, from evidence on the developer's own machine. This roadmap
communicates priorities; it is not a promise of dates or release numbers.

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

## Current capabilities

### Released (v0.3.3)

- Runtime collection from processes TailFlow starts, local Docker containers,
  log files and piped stdin, in one ordered stream.
- MCP tools and the `tailflow-logs` shell client; error grouping; server-side
  waits; cursors for before/after comparisons.
- The live-log dashboard, the terminal UI and `tailflow init`.

### On `dev` for v0.4.0 (upcoming)

- Durable SQLite history with retention, size cap, redaction, capture modes
  and an explicit completeness verdict.
- Opt-in Claude Code and Codex hooks: sessions, tool calls, commands, reported
  file changes and subagents, with stated coverage.
- The agent dashboard: overview, sessions, activity search, multi-agent
  timeline, files, commands and live logs, updated from a stream of committed
  events.
- Opt-in file observation and Git snapshots compared with what agents
  reported; command outcomes as reported; related evidence in five tiers that
  never claim cause.
- An authenticated local API: a per-run token for clients and a one-time
  sign-in link for browsers.

## Being validated before v0.4.0

- A live Codex session against the hook integration (so far tested with
  synthetic hook input only).
- Real Claude Code and Codex sessions on Windows (so far CI with synthetic
  hook input).
- A 24-hour endurance run on Linux.
- Benchmarks on a quiet, dedicated host, and on Linux.
- Upgrading `ratatui` in the terminal UI to clear the `lru` advisory
  ([security](security.md#dependency-advisories)).

## Planned

None of these is a commitment, and none has a date.

### Agents and evidence

- More agent integrations.
- Explicit command-to-runtime correlation using request, trace or job
  identifiers, instead of closeness in time.
- A verified exit status for commands TailFlow itself launches.
- Stronger multi-agent investigations, such as comparing what two sessions
  touched in one repository.
- Resource usage per session.

### Verification workflow

- Compare recent error groups against a baseline cursor: "what is failing now
  that was not failing before this edit?"
- Clearer startup and readiness signals for configured sources.
- A readable query language (`agent:claude type:file.modified since:24h`)
  compiling to the existing structured filters.
- Exports and diagnostics bundles for sharing an investigation.

### Setup and sources

- Optionally let `tailflow-mcp` start a local daemon when `tailflow.toml`
  exists and none is running.
- Serve MCP directly from the daemon over streamable HTTP.
- Understand Docker Compose projects and services, with include/exclude
  filters for containers.
- Better fingerprints for common runtime formats (Rust panics, Java
  exceptions, Go panics, JavaScript stack traces).

### Reliability

- Longer automated reliability runs on Linux, macOS and Windows.

## Exploratory ideas

These may be useful but need stronger evidence before becoming plans:

- pluggable local sources such as Kafka or Redis pub/sub;
- a local webhook source for development log drains;
- sources exposed as MCP resources as well as tools;
- side-by-side source panes and log-level toggles in the TUI; and
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
