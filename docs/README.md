# TailFlow documentation

Use this page to find the shortest path to the information you need.

## I want to…

| Goal | Start here |
|---|---|
| Install TailFlow and see my first logs | [Getting started](getting-started.md) |
| Connect an MCP client | [Getting started: connect an agent](getting-started.md#connect-an-agent) |
| Understand what TailFlow can do | [Features](features.md) |
| Learn the MCP tools | [TailFlow for agents](agents.md#mcp-tools) |
| Use TailFlow from a shell or script | [TailFlow for agents: shell client](agents.md#shell-client) |
| Call the HTTP API | [TailFlow for agents: HTTP API](agents.md#http-api) |
| Use the web dashboard and read what it shows | [Dashboard](dashboard.md) |
| Keep history, set retention, or control what is recorded | [Durable history](storage.md) |
| Sign in to the dashboard, use the API token, understand who can read the API | [Security](security.md) |
| Compare what agents report with what changed on disk and in Git | [Evidence](evidence.md) |
| Understand the internals | [Architecture](architecture.md) |
| Evaluate production or automation use | [Limitations](limitations.md) |
| See planned work | [Roadmap](roadmap.md) |
| Contribute code | [Contributing](../CONTRIBUTING.md) |

## Documentation map

### [Getting started](getting-started.md)

Installation, source configuration, MCP registration, human interfaces, and
common setup problems. This is the best first guide for a new user.

### [Features](features.md)

Capabilities organized around the runtime-verification loop, with use cases,
interface availability, and the evidence each feature can and cannot provide.

### [TailFlow for agents](agents.md)

Reference for the four MCP tools, reliable agent workflow patterns, the
`tailflow-logs` CLI, cursor behavior, and HTTP response shapes.

### [Agent capture](agent-capture.md)

Recording Claude Code and Codex sessions, tool calls, commands and file edits
through opt-in hooks: setup, verification, exactly what each integration can
and cannot see, privacy, loss behavior, and uninstalling.

### [Dashboard](dashboard.md)

The local web dashboard: overview, sessions, activity search, the multi-agent
timeline, files, commands and live logs; how to read agent-reported data,
coverage and loss; keyboard use, cost and security.

### [Durable history, retention, and privacy](storage.md)

Where history is stored, durability and completeness guarantees, retention and
size caps, capture modes and redaction, health inspection, purging, and the
`/api/v1` endpoints.

### [Architecture](architecture.md)

How ingestion, buffering, presentation, and packaging fit together, including
the repository layout and important design invariants.

### [Limitations](limitations.md)

The local-only security model, retention, heuristic classification and redaction,
supported sources and platforms, and what TailFlow deliberately does not do.

### [Roadmap](roadmap.md)

Near-term priorities, later opportunities, exploratory ideas, and non-goals.

## Versioning

The documentation on `main` describes the latest release. See the
[changelog](../CHANGELOG.md) for behavior introduced in earlier versions.
