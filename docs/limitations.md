# Limitations and operational boundaries

TailFlow is designed for fast local development feedback. Understanding its
boundaries prevents an agent or script from treating partial evidence as proof.

[Documentation index](README.md)

## Scope at a glance

| Area | Current boundary | Practical implication |
|---|---|---|
| Network | Loopback-only HTTP; token or session required, no TLS | Do not expose the daemon directly to a network |
| Live buffer | Bounded in-memory ring per daemon run | MCP tools and `/api/query` see only the current run |
| Durable history | Local SQLite, 30 days / 2 GB by default | Events in flight at `kill -9` are lost (reported as a gap) |
| Redaction | Heuristic credential patterns | Not every secret is detected |
| Agent activity | Opt-in Claude Code and Codex hooks (next release) | Only what the agent's hooks report; shell side effects, Claude Code tokens and agent versions are not seen |
| File and Git evidence | Opt-in `[observe]` directories (next release) | Shows what changed on disk and in the worktree, never who changed it; a watcher can miss or coalesce notifications (loss is recorded when the OS reports it) |
| Data type | Text logs | No traces, metrics, profiles, or state inspection |
| Classification | Text and JSON severity heuristics | Custom formats may receive the wrong level |
| Grouping | Normalized line fingerprints | Similar errors may merge; variable shapes may split |
| Docker | All running containers on the local daemon | No Compose-service allowlist, Kubernetes, or remote Docker target |
| Files | Local paths | No network log drains or object-store ingestion |
| Platforms | macOS ARM64/x64, Linux ARM64/x64, Windows x64 | Other targets require a source build and may be untested |

## Security model

The daemon binds to `127.0.0.1`, rejects nonloopback Host headers, does not
enable cross-origin access, and requires a credential on every data
endpoint: a per-run API token (CLI, MCP, scripts) or a same-origin browser
session (see [security](security.md)). It has one credential level (full
read access), no per-user permissions, and no transport encryption.

That is appropriate for one developer machine. It is not sufficient for:

- a shared development server;
- direct access from another machine;
- an internet-facing endpoint; or
- multi-tenant log access.

Logs can contain credentials, tokens, personal data, and customer content.
TailFlow redacts common credential formats (API keys, bearer tokens, URL
passwords, private keys, `password=` assignments) before display and before
storage, unless `capture = "full"`. That is a heuristic: personal data,
customer content, and secrets in unrecognized formats pass through.

Other OS users, containers sharing the host network and web pages on other
localhost ports cannot read the API without a credential. Processes running
as your own user can: they can read the token file and the database
directly. Browsers send loopback cookies to every port, so a malicious
server on another loopback port that your browser visits receives the
session cookie (see [security](security.md#outside-the-threat-model)). No
HTTP endpoint can delete or change data. The database file is protected only
by filesystem permissions (a user-only directory on Unix) and is not
encrypted at rest.

If remote access is required, place an authenticated, encrypted proxy in front
of TailFlow and evaluate the exposure deliberately. Remote operation is not a
tested first-class workflow.

## Retention and completeness

The live ring retains the newest `--buffer` records (default 5,000), up to
`--buffer-mb` of log text (default 16 MiB; reported as `buffered_bytes` on
`/health`) — with very long lines the byte limit is reached first — and
restarts its cursor at each daemon start. Single lines longer than 64 KiB are
cut and marked `… [tailflow: line truncated from N bytes]`. Durable history is separate: SQLite,
30-day retention, 2 GB cap, persistent cursors. See
[durable history](storage.md) for its guarantees. In particular:

- an event counts as persisted only after its batch commits; with the default
  `synchronous = "normal"` the newest commits can be lost on power failure;
- the size cap applies to live database pages; the WAL and not-yet-reclaimed
  free pages can briefly add to the file size;
- the TUI does not write history;
- one database is shared by all your daemons by default, so history from
  other projects (recorded under their own privacy settings) is visible unless
  you filter by `project` or give each project its own `[storage] path`; and
- time during which no daemon ran is not recorded as a gap; and
- on Windows there is no cheap process-liveness probe, so a daemon that was
  killed (not stopped cleanly) is recognised only once its heartbeat is more
  than 60 s old — at the next start, or at the next full cleanup pass of a
  running daemon. Until then the up to ~0.5 s of events it had not yet
  committed are not reported as a gap. On macOS and Linux the next start
  detects it immediately.

Responses expose `buffer_start_cursor`, `next_cursor`, `buffer_starts_at`, and
`cursor_gap` where relevant. Callers should treat `cursor_gap: true` or a short
buffer horizon as incomplete evidence—not as proof that nothing happened.

Increasing the buffer improves the retrospective window but uses more memory.
It does not create durable history.

## Severity inference

TailFlow prefers recognized fields in structured JSON and otherwise detects
common level tokens in text. A line without a recognized level is `unknown`,
which ranks below `trace`.

Consequences:

- custom severity names can remain `unknown`;
- prose containing words such as “error” can occasionally be ambiguous;
- stack frames normally have no severity and are recovered as trailing context,
  not independent errors; and
- a source that prints nothing can be healthy, stuck, or not yet initialized.

Always use source lifecycle state alongside error queries when deciding whether
the stack is healthy.

## Failure grouping

Deduplication replaces changing values such as numbers, timestamps, UUIDs,
addresses, and quoted strings before comparing lines. This is intentionally
lossy compression for an agent context window.

For example, missing-module errors containing different quoted module names can
group together. Conversely, the same logical failure expressed with materially
different wording can appear as separate groups. Use `search_logs` when exact
payloads or ordering matter.

## Source behavior

### Processes

Process commands run through the platform's native shell and inherit TailFlow's
working directory and environment. Relative commands and file paths are not
rebased to the directory containing `tailflow.toml`. TailFlow captures
stdout/stderr and attempts to terminate the full process tree on shutdown. It is
a development supervisor, not a production init system: it does not provide
resource limits, health checks, dependency ordering, or durable restart
accounting.

### Docker

Docker discovery follows all currently running containers and checks for
changes every two seconds. Container selection, Compose project awareness,
Kubernetes pods, and remote container runtimes are not implemented. Attaching to
a container starts with its last 50 Docker log records, so TailFlow is not a
complete container log archive.

### Files

File sources follow appends and handle a missing initial file, replacement,
rotation, and truncation. They do not recursively watch directories, expand
globs, parse compressed rotations, or read historical rotated files.

### Standard input

Piped input belongs to that TailFlow process. It cannot be recovered or shared
after the process exits unless another source also writes it to a file.

## Agent and API boundaries

- MCP uses stdio. The daemon does not yet expose MCP directly over streamable
  HTTP.
- `tailflow-mcp` does not currently start the daemon automatically.
- Queries are read-only and operate only on captured logs. TailFlow does not
  restart services or execute remediation actions through its API.
- `wait_for_logs` proves that a matching line appeared. A timeout proves only
  that no retained/new line matched before the deadline.
- The MCP output is optimized for bounded context, so callers needing exact data
  should request JSON or use the HTTP API.

## Agent capture boundaries

See [agent capture](agent-capture.md#what-tailflow-cannot-see) for the full
matrix. In short:

- Events are **agent-reported**. A `file.modified` means the agent's file tool
  reported an edit, not that TailFlow saw the file change. Edits made through
  the shell (`sed -i`, `>`, `mv`, build scripts) are not recorded.
- **Claude Code** reports a shell command's exit code only when it fails
  (parsed from the documented `Exit code N` error text). **Codex** documents
  no exit code at all, so its commands have no recorded outcome unless the
  response happens to carry one.
- Codex runs hooks only after you trust them in `/hooks`; Claude Code
  interactive sessions run hooks only in trusted folders; managed policy can
  disable user hooks entirely. TailFlow reports what it can detect but cannot
  override any of these.
- A hook call costs the agent about 15 ms (two per Claude Code tool call),
  almost all of it process start.
- Without a running daemon, hook calls wait in the spool (about 20,000 calls).
  Beyond that they are dropped and counted as a recorded gap.
- Git association reads `.git` metadata at session start and end. A HEAD that
  moved during a session is reported as inferred, low confidence, and never
  attributed to the agent.
- Codex support is built from Codex's published hook schemas and synthetic
  fixtures; it has not yet been exercised against a live Codex install.
- Codex runs hooks through your login shell (`$SHELL -lc`). Anything your shell
  profile prints at startup becomes hook output, which Codex adds to the
  model's context on `SessionStart`. TailFlow cannot prevent that; keep the
  profile quiet for non-interactive shells.
- Agent events are ingested only into the default per-user database; a daemon
  with a custom `[storage] path` / `--db` does not ingest them.

## File and Git evidence boundaries

- Only directories listed in `[observe]` (or `--observe`) are watched, and only while a daemon runs. A change made while nothing was observing it is **unverified**, not absent.
- A file watcher records that a path changed, not which process changed it. A **reported + observed** match is an association within a time window (± 5 s by default), not authorship.
- The OS may coalesce or drop notifications. Drops the OS reports (`fs_watcher_rescan`) and TailFlow's own queue overflows (`fs_observer_overflow`) are recorded as loss; drops the OS does not report cannot be detected.
- On Linux at most `max_directories` directories per root are watched (inotify uses one watch per directory); beyond that coverage is reported as partial and reports there are **unverified**.
- Git snapshots list at most 150 changed paths under the observed directories (the total is always recorded); a session comparison stops at the last listed path and is marked incomplete. Snapshots happen at startup, at session boundaries (seen within about a second) and after changes settle (at most every 30 s by default): changes between snapshots are seen only as their net effect.
- TailFlow runs `git status` and `git diff --numstat` read-only, with every filter driver disabled. Files managed by a filter (for example Git LFS) can therefore appear modified in snapshots when your own `git status` shows them clean.
- `.gitignore` support is a subset (no `[[:class:]]`), and ignore files created after the observer started apply from the next start.

## Outcomes and correlation boundaries

- A command's outcome is what the agent reported. TailFlow does not run, trace or observe agent processes, so `verified_exit_status` never appears for them. Codex reports an outcome only when its tool response carries an exit code.
- Classification uses the program, a vocabulary subcommand and a pattern-matched script name. Commands recorded before this release, wrappers TailFlow does not know, and custom script names are `unknown` or `other`.
- "Nearby" runtime errors are close in time only. The one inference rule (an error message naming a changed file) is a heuristic and is labelled as such.

## Dashboard boundaries

See the [dashboard guide](dashboard.md) for how each view reads its data.

- Agent activity streams to the dashboard only after it is committed to
  durable history. Expect about 1 second from a hook call to an updated page
  (spool pickup, the storage batch delay, a 250 ms coalescing window, then
  the page's own query). Commits made by *another* daemon sharing the
  database are seen within 250 ms of committing (polling interval). A
  reconnecting tab catches up at most 5,000 events, then reloads its views
  instead.
- The live log view keeps the newest 5,000 lines and renders only the rows on
  screen. Browser find-in-page and drag-selection cover only rendered rows;
  use the filter box, **Copy lines**, or Activity search instead.
- A timeline window draws at most 3,000 agent events and 500 runtime markers
  and says when a window holds more. Activity and session lists load pages of
  200–500 and stop after 5,000 (Activity) or 20,000 (one session) rows in the
  browser; narrow the filters to see older events.
- Result counts in Activity are "loaded so far", not a total: counting every
  match of an arbitrary filter would mean scanning history.
- Overview counts come from indexes and are exact for retained history. When
  the range reaches back before retention, or overlaps recorded loss, the page
  says the numbers are lower bounds.
- The Integrations page reports hooks recorded by `tailflow agent install`.
  Hooks added to an agent config by hand are not detected there;
  `tailflow agent status` checks the default config paths.
- Nearby runtime events in the inspector are matched by time (±60 s) only.
  The dashboard does not infer causation.
- The live view keeps up with 1,000 lines/s in testing. Far beyond that (tens
  of thousands of lines/s), parsing the stream saturates the browser tab; the
  daemon reports the lines it dropped for that tab; durable history, when
  enabled, still records them.

## Non-goals

TailFlow is not currently intended to replace:

- production log storage and search;
- observability backends and distributed tracing;
- process managers or container orchestrators;
- test runners and CI systems; or
- security monitoring and audit logs.

These boundaries keep the product focused on its core job: giving a local coding
agent timely, compact evidence from the software it is changing.

See the [roadmap](roadmap.md) for areas that may expand over time.
