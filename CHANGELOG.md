# Changelog

All notable changes to TailFlow are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0.0, minor bumps may include changes to the Rust
crate APIs; the HTTP API and CLI surfaces are additive within a minor series.

## [0.4.0] - [Unreleased]

Milestone M1: durable event engine and observability foundation. TailFlow now
keeps a local, private, queryable history and states explicitly when that
history is incomplete.

Milestone M2: agent capture and session tracking. With opt-in hooks, TailFlow
records what Claude Code and Codex report doing — sessions, tool calls, shell
commands, the files their tools read or edit — and says what it cannot see.

Milestone M3: agent observability dashboard. The local web UI becomes an
investigation tool for agent activity and the runtime around it, without
claiming more than the hooks reported.

Milestone M4 (in progress): verified changes, runtime correlation and local
API security.

### Added

- **Durable local history.** `tailflow-daemon` records runtime logs and
  collector lifecycle in a local SQLite database (WAL, bundled SQLite 3.54,
  FTS5) in the user data directory. Defaults: 30-day retention, 2 GB size cap,
  7-day retention for optional payload bodies. New `tailflow-storage` crate.
- **Versioned event model** (`ObservabilityEvent`, schema v1) with provenance
  (`agent_reported`, `filesystem_observed`, `runtime_observed`,
  `system_observed`), an event-type catalog covering runtime, agent, file,
  command, git, and system events, strict validation, and replay-safe
  deduplication for producers with stable ids. Agent, file, command, and git
  collectors are **not** part of this release; the types are schema only.
- **`/api/v1` read API:** `events` (filters by type or family, source, session,
  agent, project, severity, status, path glob or prefix, command, full text,
  and time; seq cursors in both directions), `events/{seq}` (`404` vs `410`
  expired), `storage/status`, `storage/retention`, `collectors/status`,
  `schema`, and `privacy`. Unknown parameters are `400`s.
- **Completeness on every history response:** recorded gaps for ingestion-bus
  lag, storage-queue overflow, failed commits, unclean shutdowns (unknown
  count), and deleted sessions, plus retention horizons. Responses are never
  marked complete after detected loss in the scanned range.
- **Privacy controls:** `[privacy]` capture modes `metadata` (default),
  `redacted`, and `full`; built-in redaction of common credential formats with
  multi-line private-key handling; custom rules and rule exclusions; event-type,
  agent, path-glob, and project-directory collection boundaries.
- **`tailflow-daemon storage path|status|check|purge`.** Purge (all, before a
  time, or one session) requires `--yes` and is safe while a daemon runs.
- `--no-storage` and `--db PATH` daemon flags; `[storage]` and `[privacy]`
  config sections (strict: unknown keys are errors).
- Dashboard **diagnostics** panel and a banner when the live view missed
  records.
- `Source::kind()` (default method) so persisted events record their collector
  family.

- **Agent capture (M2).** `tailflow agent install|uninstall|status|list` for
  Claude Code and Codex. Installs observe-only hooks into the agent's own
  configuration (Claude Code `settings.json` at user, project or local scope;
  Codex `hooks.json`, events chosen by the detected Codex version),
  idempotently, preserving every other setting and hook, with a backup and an
  atomic write. Uninstall removes only TailFlow's handlers.
- **Hook bridge** `tailflow agent hook <agent>`: reads the hook input (bounded
  to 8 MiB), normalizes it, applies the per-user agent capture policy
  (`<data dir>/agents/config.toml`, same keys as `[privacy]`) and writes an
  owner-only spool record. Prints nothing, always exits 0, never touches the
  database or network; ~15 ms per call, nearly all process start.
- **Normalized agent events**: `agent.session.*`, `agent.tool.*`,
  `command.*`, `file.read|created|modified|deleted|renamed` (agent-reported,
  never claimed as verified on disk), and new types `agent.subagent.started`,
  `agent.subagent.stopped`, `agent.prompt.submitted` (character count only),
  `agent.turn.completed`, `agent.turn.failed`, `agent.model.completed` (Codex
  token counts). Source ids are namespaced (`claude-code:<id>`); ids derived
  from tool-call identity deduplicate replays and duplicate registrations.
- **Sessions**: `tailflow sessions`, `tailflow session <id> [--events]` (work
  without a running daemon), `GET /api/v1/sessions`,
  `GET /api/v1/sessions/{id}`, `GET /api/v1/agents/status`. Each session has a
  derived status (`active`, `ended`, `unterminated`), lifecycle coverage,
  recorded loss, retention truncation, the integration's capability limits,
  and tool / command / failure / file summaries. Git branch and HEAD at start
  and end come from `.git` files (no `git` process).
- **Daemon spool ingestion**: an event-driven thread moves spooled agent
  events into history (~0.2 s), deleting a record only after its events are
  committed. `--agents` runs the daemon with no log sources (implied when an
  integration is installed); `--no-agents` turns ingestion off.
  `tailflow-daemon agent ingest` drains the spool once.
- Gap reasons `agent_spool_overflow`, `agent_input_rejected`,
  `agent_spool_corrupt`; `health.events_dropped.agent`.
- **Agent observability dashboard** ([guide](docs/dashboard.md)): Overview
  (index-backed counts, histogram, per-agent status and coverage limits,
  recent sessions and failures), Sessions (search, filters, cursor
  pagination, detail with git boundaries, coverage and events), Activity
  search (FTS + structured filters, `key:value` syntax, filter chips,
  copyable URLs, event inspector), multi-agent Timeline (agent lanes and a
  runtime lane; zoom, pan, brush), Errors, Files, Commands, Integrations and
  Settings (dark / light / system theme, local / UTC time). Keyboard
  navigable, responsive down to phone width.
- **Read-only aggregate endpoints** for the dashboard:
  `GET /api/v1/stats` (sessions by status, tool calls, commands, distinct
  failed operations, reported file activity, runtime errors and an event
  histogram, from indexes, under the 3 s query budget) and
  `GET /api/v1/files` (agent-reported file operations grouped by path,
  `multi_agent=true` for paths more than one agent touched).
- `GET /api/v1/sessions`: `until`, `q` (substring of id, project, directory,
  branch), and cursor pagination (`cursor` → `has_more`, `next_cursor`).
  Session counts gain `failed_operations`.
- `GET /api/v1/events?program=` filters agent command events by program.
- `GET /api/v1/agents/status` reports each integration's installation state
  (`configured`, `incomplete`, `not_installed`, `unknown`) by reading the
  install record and the agent's config read-only.
- `GET /api/records?after=<seq>` and `instance_id` in `/health`, so live
  clients catch up incrementally and detect a daemon restart.

- **Local API authentication (M4-A).** Every data endpoint requires a
  credential (see `docs/security.md`). The CLI, MCP and scripts use a per-run
  256-bit API token that the daemon writes to an owner-only runtime file,
  `<data dir>/run/daemon-<port>.json`, or `$TAILFLOW_TOKEN`. Browsers use an
  `HttpOnly; SameSite=Strict` session cookie created by a one-time sign-in
  link. New commands: `tailflow-daemon dashboard` (open the browser with a
  fresh link; `--no-open`, `--revoke-sessions`) and `tailflow-daemon token`.
  New endpoints: `GET /auth/login/<code>`, `GET /api/v1/auth/session`,
  `POST /api/v1/auth/login-links`, `POST /api/v1/auth/sessions/revoke` (both
  token-only) and `POST /api/v1/auth/logout`. The dashboard has a sign-in
  screen and **Settings → Access → Sign out**.

- **Live agent activity (M4-B).** New `GET /api/v1/stream/agent` (SSE).
  - It carries agent, command, file and git events once they are committed
    to durable history, in seq order, each with its seq as the SSE `id`.
  - Clients resume from `after=` or `Last-Event-ID`, with catch-up from
    SQLite (bounded at 5,000 events, then a `reset`).
  - Detected loss and expired history arrive as `gap` events. A lagging
    client is re-read from history rather than dropped (`lag`).
  - One shared feed per daemon. It wakes on this daemon's commits and polls
    the newest seq every 250 ms for other daemons' commits, only while
    someone is subscribed.
  - The dashboard replaces 10–30 s polling with targeted, coalesced reloads,
    and adds an **Agents** stream pill in the top bar.

- **Verified file changes (M4-C).** Opt-in `[observe] paths` / `--observe
  DIR` watches selected project directories. It records `file.*`
  observations (provenance `filesystem_observed`, no contents) and
  compares them with agent-reported changes: `matched_observation`,
  `reported_only`, `unverified`, `filesystem_observed`, `ambiguous`.
  - Built-in, `.gitignore`, editor temporary file and configured
    exclusions apply; symlinks are not followed.
  - On Linux there is a directory watch budget, and coverage beyond it is
    reported as partial.
  - Coverage is recorded per directory and run, and one daemon observes a
    directory at a time.
  - New loss reasons: `fs_observer_overflow` and `fs_watcher_rescan`.
  - `GET /api/v1/observe/status` and `GET /api/v1/evidence/files`;
    **Files → Verified on disk**; **Integrations → File & Git observation**.
- **Git change evidence (M4-D).** Read-only, bounded `git status` and
  `git diff --numstat` snapshots of observed repositories, taken at
  startup, at agent session boundaries and after changes settle. They
  produce `git.snapshot`, `git.commit`, `git.checkout` and `git.head.moved`
  events.
  - `GET /api/v1/sessions/{id}/changes` compares a session's worktree
    before and after. It gives each changed path its reported/unreported
    evidence, never credits pre-existing changes, and lists concurrent
    sessions and limitations.
  - **Session → Git changes**.
- **Command outcomes (M4-E).** Hooks also record a command's intent:
  - the effective program, for example after `cd …&&`;
  - a subcommand from a fixed per-tool vocabulary;
  - a script name matching a fixed pattern.

  Arguments are never recorded. `GET /api/v1/outcomes` and **Commands →
  Tests, builds & checks** classify commands (build, test, lint, typecheck,
  format, deps), with the rule that decided each one. Outcomes are only
  what was reported (`reported_success`, `reported_failure` with the exit
  code, `interrupted`, `outcome_unknown`). Runtime errors during or up to
  30 s after a command are listed as nearby. Session summaries list tests,
  builds and checks.
- **Evidence-graded correlation (M4-F).** `GET /api/v1/events/{seq}/related`
  and the inspector's **Related evidence** list related events in five
  tiers (direct identifiers, independent observation, shared context,
  temporal proximity, inference), each with its reason, and no causal
  claims.
- Dashboard: provenance markers (**on disk**, **git**) in event lists, and
  route-level code splitting (initial script 52.6 → 33.8 KB gzip).
- Schema v4: `fs_coverage` and the partial indexes `events_path_ts` and
  `events_fs_ts`. A large database prints progress around the one-time
  upgrade.

### Changed

- **Breaking: the HTTP API requires a credential.** `tailflow-mcp` and
  `tailflow-logs` from this release find the token by themselves. Older
  clients and plain `curl` get `401` and must send
  `Authorization: Bearer $(tailflow-daemon token)`. `/health` without a
  credential returns only `ok`, `version` and `auth_required`.
  `tailflow-logs` exits `4` when not authorized. `npm run dev` proxies with
  the token server-side, for same-origin requests only.
- **The live log view is virtualized.** Only on-screen rows are rendered
  (variable height, expanded JSON, wrapping or horizontal scrolling), the
  view stays put while paused or scrolled up, and a reconnect catches up
  once from where the stream broke instead of re-reading the whole buffer.
  A background tab no longer accumulates unbounded pending records.
  Measured with 5,000 lines held while streaming 1,000 lines/s (headless
  Chrome): DOM nodes 25,039 → 467, main thread ~99 % → ~19 %, frame p95
  167–300 ms → 17 ms.

- **Live output is redacted by default.** Redaction runs once at ingestion, so
  the dashboard, SSE, `/api/records`, `/api/query`, `/api/errors`, MCP, and the
  database all see `[REDACTED:<rule>]` in place of matched secrets. Set
  `capture = "full"` to restore verbatim output.
- Ingestion-bus overflow, previously only a log line on the daemon's stderr, is
  now counted and surfaced: `ingest_dropped` on `/health`, `/api/sources`,
  `/api/query`, `/api/errors`, and `/api/wait` (additive field), a
  `[tailflow] N log records were dropped` warning in the live stream, and a
  durable gap.
- SSE subscribers that fall behind receive a named `gap` event; clients that
  listen only for `message` events are unaffected.
- `tailflow-mcp` and `tailflow-logs` warn when the daemon dropped records, and a
  wait timeout after loss no longer claims the condition "did not occur".
- The daemon handles `SIGTERM` like Ctrl-C and drains durable storage before
  exiting.
- The daemon applies `--grep`/`--source` in its single ingestion task instead
  of a second broadcast hop, removing one point where records could be lost.
- **Lower resource use (M1.5).** Durable storage commits every 500 ms (was
  100 ms) or 512 events, cutting SQLite write volume by about two thirds at
  typical event rates; `/api/v1/events` history trails live ingestion by up to
  ~0.5 s (live endpoints are unaffected; `batch_max_delay_ms` restores the old
  value). The in-memory buffer is also bounded by bytes (`--buffer-mb`,
  default 16 MiB; `buffered_bytes` on `/health`), records are shared between
  the buffer and every SSE subscriber instead of copied, level detection no
  longer allocates per line, idle history-query connections give back their
  cache, a quiet tailed file is polled every 2 s instead of every 250 ms, and
  release binaries are built with LTO and stripped symbols (31–37 % smaller).
  The dashboard re-renders only new rows.
- **Minimum supported Rust version is now 1.89** (was 1.88), required by
  rusqlite 0.41 for the bundled SQLite security fix below.

- **Storage schema v2**: adds the `agent_sessions` table and drops the
  never-used `events_ts` index (5–8 % fewer WAL bytes per event). Migrates in
  one transaction; schema-v1 builds refuse a v2 file without modifying it.
- **Storage schema v3**: a partial covering index over agent events
  (`events_agent_activity`) for the dashboard aggregates. Runtime rows are not
  indexed; the first open after upgrading builds it once (≈ 25 s for 2 M
  events on a heavily loaded machine).
- **Storage maintenance yields to ingestion**: retention stops early while the
  storage queue is over a quarter full, and the full-text compaction is
  postponed (or interrupted past half full) and retried; after 60 s of
  deferral it runs regardless, so cleanup always progresses.
  `health.maintenance` reports yields, deferrals and forced runs.
- `[privacy] excluded_agents` accepts `claude-code` (alias `claude`) and
  `codex` and rejects other names at startup, instead of silently excluding
  nothing.
- `[privacy] allowed_project_dirs` and `excluded_file_globs` now resolve `.`
  and `..` before matching, so `/work/../etc/passwd` no longer passes an
  allowlist of `/work`.
- The per-user data-directory helpers moved from `tailflow-storage` to
  `tailflow-core::paths` (re-exported from the old location).
- The `tailflow` binary no longer starts its async runtime for subcommands.

### Fixed

- A hash change that happened between the dashboard's first render and its
  router effect was missed, leaving the previous page shown (more likely
  since the sign-in check delays the first render).

- Ctrl-C or `SIGTERM` did not stop the daemon while any client was connected
  to the live stream (`/events` — an open dashboard tab, `curl`, an agent): the
  HTTP server waited for that never-ending stream, so storage was never
  drained and the eventual forced kill was recorded as an unclean shutdown.
  Live streams now end when shutdown begins, and a pending `/api/wait` returns
  `503` ("the daemon is shutting down") instead of waiting out its timeout.
- Under continuous size eviction the full-text index kept the entries of
  deleted events in segments that were almost never merged, and grew to
  between 3 and 13 times its compacted size. Because the size cap counts it,
  the database typically held about 40 % fewer events than it could (at
  times about 65 % fewer). A cleanup cycle that removes events now ends with
  one bounded merge (about 64 pages) of the index segments that consist mostly
  of deleted entries; the index stays close to its compacted size without
  extra work during ingestion. Retention reports carry `fts_compacted`.
- On Windows the daemon now stops cleanly — draining storage and closing the
  run — on Ctrl+Break (how supervisors stop console processes), not only on
  Ctrl+C, and attempts the same when its console window is closed (Windows
  allows about 5 seconds). Previously all of these ended it as an unclean
  shutdown.
- On Linux, tailing a file kept one CPU core busy for as long as the daemon ran:
  the collector re-read the file on inotify *access* events, and its own
  re-read produced the next one. Access events are now ignored.
- A process, stdin, or tailed file that wrote a non-UTF-8 byte stopped being
  captured (process and stdin) or failed its collector (file), silently. Invalid
  bytes are now replaced with U+FFFD and capture continues.
- Collectors buffered a line without a size limit. Lines are now capped at
  64 KiB and marked `… [tailflow: line truncated from N bytes]`; the cut drops
  the token it lands in so no fragment of a secret escapes redaction. A cut
  JSON line is classified by its text, since it no longer parses.

### Security

- Local API access control: other OS users, containers on the host network
  and web pages on other localhost ports can no longer read agent sessions,
  paths, commands or logs. A session cookie counts only on requests the
  browser marks `Sec-Fetch-Site: same-origin` (or `none` for a URL the user
  opened), because browsers send loopback cookies to every port. The cookie
  name carries the port, and tossed duplicate cookies are tolerated. Every
  protected path refuses with the same answer whether or not the resource
  exists, so there is no existence oracle. Tokens and sessions are compared
  in constant time, and refused requests are counted in the authenticated
  `/health`.
- Browser requests with a non-loopback `Origin`, and cross-site requests other
  than top-level navigation (`Sec-Fetch-Site`), are refused with `403`. Request
  targets over 8 KiB are refused with `414`. No HTTP endpoint mutates or deletes
  data.
- New database files are created owner-only (`0600`) on Unix, inside an
  owner-only data directory.
- `/api/v1/events` pages are bounded to about 8 MiB of text and to 100 events
  when `include_payload=true`.
- Removed the unmaintained `atty` dependency (RUSTSEC-2021-0145,
  RUSTSEC-2024-0375); terminal detection uses the standard library.
- Bundled SQLite updated to 3.54.1 (rusqlite 0.41.1, libsqlite3-sys 0.39.1)
  for CVE-2026-119375 / RUSTSEC-2026-0459: a database opened together with a
  crafted `-wal` file could return uninitialized memory.
- `url` 2.5.9 (RUSTSEC-2026-0592; not reachable from TailFlow), `tokio`
  1.51.6, `rustix` 1.1.6 and `anyhow` 1.0.103 (unsoundness advisories).
  `cargo audit` reports no vulnerabilities.

- The embedded UI is served with a strict Content Security Policy
  (`script-src 'self'`, no inline script, `frame-ancestors 'none'`),
  `X-Content-Type-Options: nosniff` and `X-Frame-Options: DENY`.
- Web dev dependencies: `vite` 6.4.6 and `source-map-js` 1.2.2 (override)
  resolve the dev-server advisories reported by `npm audit`; the shipped
  bundle was not affected.
- Agent capture adds **no network listener and no HTTP write endpoint**: hook
  processes hand events to the daemon through an owner-only spool directory.
  Redaction and collection boundaries run in the hook before anything reaches
  disk, and again in the daemon. Prompts, assistant messages, tool output and
  file contents are never recorded; command lines only in `redacted`/`full`
  capture mode.

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
