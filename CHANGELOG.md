# Changelog

All notable changes to TailFlow are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0.0, minor bumps may include changes to the Rust
crate APIs; the HTTP API and CLI surfaces are additive within a minor series.

## [Unreleased]

## [0.3.3] - 2026-08-19

Correctness release for the agent contract. Every change here closes a path
where a malformed request could return a successful-looking empty result — the
one failure mode an agent cannot detect, because "nothing came back" and
"nothing is wrong" are the same answer.

### Fixed

- MCP tool arguments are now validated against the schema each tool advertises.
  An unknown argument name, an argument belonging to a different tool, a
  wrong-typed value, or a value outside a declared `enum` is returned to the
  model as a tool error instead of being dropped. Previously a misspelled
  `pattern` or a non-string `grep` was silently ignored and the query ran
  unfiltered, so a stack full of errors could answer as though it were clean.
  Rejected arguments are reported with the accepted names and a "did you mean"
  suggestion, and validation runs before the daemon is contacted.
- Malformed query-string values on `/api/query`, `/api/errors`, and `/api/wait`
  now return the same `{"error": "..."}` JSON body as every other rejected
  argument. They previously fell through to the framework's plain-text
  rejection, which a caller reading the `error` field would find empty.
- `limit=0` and `max_payload_chars=0` are rejected rather than quietly raised to
  1. A clamped-up zero returns a single record that looks like a deliberate
  answer, and there is no truncation flag that can say otherwise.
- `wait_for_logs` no longer presents a pre-existing log line as a fresh event.
  A wait is also satisfied by a match already held in the buffer, so
  `wait_for_logs(grep: "compiled successfully")` could return the *previous*
  build's success line, report `waited_ms: 0`, and render as "Matched after
  0ms" — reading exactly like the caller's own change compiling. Responses now
  carry `matched_from_buffer`, and the text rendering states that the line
  predates the wait, dates it, and says how to require a new event.
- A wait that fell behind the broadcast bus could recover by anchoring to a
  record that predated the call. Recovery now searches forward of the call's
  snapshot only.
- `context_lines` above the maximum is rejected rather than clamped. A group
  carrying 50 of 200 requested stack frames is indistinguishable from a
  50-frame stack trace, so this bound cannot be reduced silently the way
  `limit` and `max_payload_chars` can — both of those announce the reduction
  through `truncated` and `payload_truncated_from`.

### Added

- `wait_for_logs`, `tailflow-logs wait --require-new`, and
  `/api/wait?require_new=true` wait strictly forward: the buffer is not
  consulted, so only a line arriving after the call starts can end the wait.
  This is the precise form of "did my change cause this?" for a caller holding
  no baseline cursor and unable to guess a `since` window. The CLI exits
  non-zero when nothing new arrives, so a shell gate cannot pass on a stale
  line. A daemon older than 0.3.3 would drop the argument silently, so the
  client checks `matched_from_buffer` in the reply and reports the version
  mismatch instead of accepting a weaker answer than it asked for.
- `wait_for_logs` and `tailflow-logs wait` accept `since`, which bounds the
  retrospective half of a wait so an old buffered line cannot satisfy it. The
  daemon already honoured `since` on `/api/wait`; only the agent surfaces were
  missing it.
- `list_log_sources` accepts `format`, matching the other three tools. It
  previously advertised no arguments, so a caller asking for JSON silently
  received text.
- Test coverage for the three agent-facing invariants: bad filters fail loudly
  and never return an empty result, an unreachable daemon is a tool error
  rather than a JSON-RPC protocol error the model never sees, and every bound
  either announces the reduction it made or refuses the request. Included is a
  test pinning the advertised tool list to the dispatch table so the two cannot
  drift.

## [0.3.2] - 2026-08-17

Setup and documentation release focused on making TailFlow easier to understand,
configure, and identify while it is running.

### Added

- Added `tailflow init`, a safe guided initializer that detects package-manager
  development scripts, Docker Compose files, and common log files; supports
  interactive selection, `--yes`, `--force`, and explicit process/file/Docker
  sources; and prints the exact commands for starting the daemon and connecting
  an MCP client.
- Added a consistent version banner to the TUI, dashboard, initializer, daemon,
  and MCP startup output while preserving clean machine-readable output.

### Documentation

- Reworked the README around the problem TailFlow solves and a shorter first-run
  path, with dedicated guides for setup, architecture, limitations, and roadmap.
- Added a task-oriented documentation index, troubleshooting guidance, honest
  operational boundaries, and complete process restart examples.
- Added a concise npm package README and aligned package metadata with the
  runtime-verification positioning.
- Reframed features around the jobs they perform in an agent verification loop,
  with a capability guide covering outcomes, use cases, interfaces, and evidence
  boundaries.

## [0.3.1] - 2026-08-17

Correctness and release-hardening follow-up to 0.3.0.

### Added

- Source lifecycle reporting (`starting`, `running`, `exited`, `failed`) so a
  configured but quiet or failed source remains visible to agents.
- Explicit cursor-horizon fields (`buffer_start_cursor`, `cursor_gap`) when an
  incremental reader falls behind the bounded ring or reconnects to a new daemon.
- MCP compatibility for legacy protocol `2025-11-25` and modern protocol
  `2026-07-28`, including discovery, response metadata and cacheable tool lists.
- CI coverage for the web production build and the declared Rust 1.88 MSRV.

### Changed

- `wait_for_logs` now uses its filter only to select the trigger, then returns
  the unfiltered event burst so stack frames and downstream lines survive.
- Docker discovery runs continuously and follows containers started or replaced
  after TailFlow launches. File tails survive rename/recreate rotation and
  truncate-and-rewrite. Spawned process trees are cancelled on shutdown and use
  the native shell on Windows.
- Severity detection uses structured JSON levels and token-aware failure terms,
  avoiding common false positives such as `0 errors`.
- The dashboard hydrates buffered records before following SSE, deduplicates by
  sequence, and reconnects after transient stream failures. The TUI pauses follow
  mode when the user scrolls and resumes with `G`.
- Agent payload requests are capped at 10,000 characters. The local HTTP server
  no longer enables permissive CORS and rejects non-loopback Host headers.
- npm releases use trusted publishing with GitHub OIDC and provenance, with the
  existing token retained only as a migration fallback. Web dependencies were
  refreshed to resolve all reported audit findings.
- Minimum supported Rust version is now stated and tested as 1.88.

### Fixed

- Unicode input can no longer panic relative-time parsing.
- SSE records carry their real sequence number, closing hydration/reconnect gaps.
- Local npm packaging now stages all four binaries and ignores every staged
  executable, preventing release artifacts from dirtying the repository.

## [0.3.0] - 2026-08-06

Repositioned around a single question: **an AI agent writes code it cannot see
running.** TailFlow already collected the logs; this release makes them readable
by an agent — deduplicated, bounded, and served over MCP.

### Added

- **`tailflow-mcp`** — Model Context Protocol server over stdio, exposing four
  tools: `list_log_sources`, `get_recent_errors`, `search_logs`, and
  `wait_for_logs`. Implements JSON-RPC 2.0 directly, with no SDK dependency.
  Register with `claude mcp add tailflow -- tailflow-mcp`.
- **`tailflow-logs`** — one-shot CLI for agents and scripts that have a shell but
  no MCP client. Subcommands `errors`, `search`, `sources`, `wait`, `status`,
  each with `--json`. Exit codes are meaningful: `0` ok (and for `wait`,
  matched), `1` bad request, `2` `wait` timed out, `3` no daemon.
- **Error deduplication by fingerprint** — identical failures are collapsed into
  one group with an occurrence count. A line's variable parts (UUIDs, hex ids,
  timestamps, addresses, numbers, quoted strings) are replaced by placeholders,
  so `postgres:5432` and `postgres:5433` group together while genuinely
  different errors stay apart. A 400-line crash loop reads as one entry.
- **Stack-trace context** — each error group carries the continuation lines that
  followed its most recent occurrence, recovered from the same source. Traces
  survive a `level >= error` filter that would otherwise drop them.
- **New daemon endpoints** — `GET /api/errors` (grouped failures),
  `/api/query` (cursor-paginated records), `/api/sources` (per-service
  counters and last line), and `/api/wait` (long-poll until a matching record
  arrives, replacing sleep-and-poll).
- **Sequence cursors** — every agent response carries `next_cursor`; passing it
  back returns exactly what has arrived since, with no gaps or double-reads.
- **Relative time filters** — `since` accepts `30s`, `5m`, `2h`, `1d` as well as
  RFC 3339.
- **`--buffer` flag** on `tailflow-daemon` (default 5000) controlling how many
  records are retained for retrospective queries.
- **`--version`** on `tailflow`, `tailflow-daemon`, and `tailflow-mcp`. Only
  `tailflow-logs` previously supported it; the Homebrew formula's test block
  asserts on `tailflow --version`, so `brew install` failed its test on every
  prior release.
- **`docs/agents.md`** — tool arguments, workflow patterns, HTTP reference, and
  the rationale behind the output format.
- `GET /health` now reports `version`, `buffered`, and `cursor`.

### Changed

- **README repositioned** around the agent use case. The TUI and web dashboard
  are unchanged and still documented.
- **Invalid filter arguments now return HTTP 400 with an explanation** instead of
  being silently ignored. A caller that cannot see the screen would read the
  resulting empty response as "all clear" and report a green build.
- `LogLevel::Unknown` now ranks *below* `trace`, so unmarked output can no longer
  satisfy a `level >= warn` query.
- The daemon's record buffer moved into `tailflow-core` as `query::LogStore`,
  which is queryable rather than append-only, and grew from 500 to 5000 records.
- `GET /api/records` responses gained an additive `seq` field. The web dashboard
  ignores it; payloads are still returned in full there, unlike the agent
  endpoints which elide at 2000 characters by default.
- npm package metadata: repository URLs pointed at a `your-org` placeholder,
  which rendered as a broken link on the npm page. Now `thinkgrid-labs`.

### Removed

- **Homebrew tap.** The formula and its release job are gone. The tap repository
  it pushed to never existed, so `brew tap thinkgrid-labs/tap` never worked;
  prebuilt archives on each GitHub release cover the same platforms without a
  second repository to keep in sync. Install via npm or direct download.

### Fixed

- npm package versions were pinned at `0.1.0` while the Rust workspace was at
  `0.2.0`. All artifacts are now bumped together.
- The npm publish steps now skip versions already on the registry, so a run that
  failed partway through can be retried. Previously the packages that had
  succeeded would abort the retry before it reached the ones that had not,
  leaving a release permanently half-published.
- Release preflight checks now report a missing or under-privileged `NPM_TOKEN`
  directly, instead of surfacing it as an unexplained `E404` from `npm publish`.

## [0.2.0] - 2026-04-05

### Added

- Homebrew formula and tap auto-update on release.
- Server-side `--grep` and `--source` filter flags for the daemon, applied
  before records enter the ring buffer.
- JSON log pretty-printing, with a TUI toggle (`p`) and expand/collapse in the
  web dashboard.

### Fixed

- Ring buffer changed from `Vec::remove(0)` to `VecDeque`, making eviction O(1)
  instead of O(n).
- TUI layout guard against zero-height terminals.
- Mutex poison recovery, so a panicked writer no longer takes the read path down.
- Filter regex is compiled once and cached rather than per record.
- Task panics are logged instead of being swallowed.
- SSE serialization errors are logged and skipped instead of killing the stream.

## [0.1.0] - 2026-04-05

Initial release.

### Added

- Unified log ingestion from Docker containers (via socket), spawned child
  processes, tailed log files, and piped stdin.
- `tailflow` — color-coded ratatui TUI with real-time regex filtering, scrolling,
  and jump-to-latest.
- `tailflow-daemon` — axum HTTP server with an SSE stream at `/events`, recent
  records at `/api/records`, and a health check.
- Embedded Preact web dashboard, served from the daemon binary via `rust-embed`.
- `tailflow.toml` configuration with auto-discovery by walking up from the
  current directory.
- npm/npx distribution via platform-specific optional dependencies for macOS
  (ARM64, x64), Linux (x64, ARM64), and Windows x64.
- CI running `fmt`, `clippy`, `build`, and `test`.

[Unreleased]: https://github.com/thinkgrid-labs/tailflow/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/thinkgrid-labs/tailflow/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/thinkgrid-labs/tailflow/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/thinkgrid-labs/tailflow/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/thinkgrid-labs/tailflow/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/thinkgrid-labs/tailflow/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thinkgrid-labs/tailflow/releases/tag/v0.1.0
