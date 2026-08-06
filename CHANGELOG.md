# Changelog

All notable changes to TailFlow are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0.0, minor bumps may include changes to the Rust
crate APIs; the HTTP API and CLI surfaces are additive within a minor series.

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

### Fixed

- npm package versions were pinned at `0.1.0` while the Rust workspace was at
  `0.2.0`. All artifacts are now bumped together.

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

[0.3.0]: https://github.com/thinkgrid-labs/tailflow/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/thinkgrid-labs/tailflow/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thinkgrid-labs/tailflow/releases/tag/v0.1.0
