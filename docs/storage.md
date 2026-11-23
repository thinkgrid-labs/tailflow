# Durable history, retention, and privacy

`tailflow-daemon` keeps a local SQLite history of what it collects, so evidence
survives a daemon restart and can be queried by time, source, type, and text.
Nothing leaves your machine, and no account or external database is involved.

[Documentation index](README.md)

## What is recorded

| Recorded today | Event types | How it is obtained |
|---|---|---|
| Every runtime log line the daemon ingests | `runtime.log`, `runtime.warning`, `runtime.error` | `runtime_observed` |
| Collector lifecycle (process, file, Docker, stdin sources) | `system.collector.started`, `.stopped`, `.error` | `system_observed` |
| Detected loss of events | `system.events.dropped` | `system_observed` |

The event schema also defines `agent.*`, `file.*`, `command.*`, and `git.*`
types. **No collector produces them yet.** They exist so future integrations
write into a stable contract; `GET /api/v1/schema` lists every type and whether
it is collected in this version.

Every event carries a `provenance` value saying how it was obtained:
`agent_reported`, `filesystem_observed`, `runtime_observed`, or
`system_observed`. Relationship fields (`session_id`, `parent_operation_id`) are
recorded as reported by the source; TailFlow does not infer them.

The TUI (`tailflow`) does not write history. Only the daemon does.

## Where data is stored

| OS | Default database |
|---|---|
| macOS | `~/Library/Application Support/tailflow/events.db` |
| Linux | `$XDG_DATA_HOME/tailflow/events.db` or `~/.local/share/tailflow/events.db` |
| Windows | `%LOCALAPPDATA%\tailflow\events.db` |

`$TAILFLOW_DATA_DIR`, `[storage] path`, or `tailflow-daemon --db PATH` override
it. On Unix the directory and new database files are created readable only by
your user.

**One database is shared by every daemon you run**, across projects. Each event
records its `project_id` (the directory containing `tailflow.toml`); add
`project=<that path>` to a query to see one project. Each daemon applies its own
`[privacy]` settings when it records, so a daemon configured with
`capture = "full"` stores verbatim lines that every other daemon's
`/api/v1/events` will then return. Give a project its own `[storage] path` if its
history must stay separate.

```bash
tailflow-daemon storage path
```

## Configuration

```toml
[storage]
enabled = true              # false (or --no-storage) = memory-only, as before
retention_days = 30         # 1..36500, or "unlimited"
max_size_mb = 2048          # always enforced, even with unlimited retention
payload_retention_days = 7  # large optional bodies expire sooner
full_text_search = true
synchronous = "normal"      # or "full"; see Durability
```

The `[storage]` and `[privacy]` sections are strict: an unknown key or an
out-of-range value stops the daemon at startup with an error naming it.

## Durability

An event is **persisted** once the SQLite transaction containing it has
committed. Ingestion hands events to a single writer thread through a bounded
queue and never waits for the disk, so a slow or full disk cannot stall live
logs.

| `synchronous` | Survives a daemon crash or `kill -9` | Survives OS crash or power loss |
|---|---|---|
| `normal` (default) | Every committed batch | All but possibly the most recent commits |
| `full` | Every committed batch | Every committed batch (slower writes) |

Events are committed in batches: every 500 ms or 512 events, whichever comes
first (`batch_max_delay_ms`, `batch_max_events`). Fewer, larger commits keep
disk writes low: each commit rewrites the same handful of index pages however
few events it carries. History queries (`/api/v1/events`) therefore trail
live ingestion by up to half a second; the live endpoints (`/events`,
`/api/query`, `/api/wait`, MCP) are not batched. Events still in the queue
when the daemon is killed are lost, and the next start records that as a gap
(see below). A clean stop (Ctrl-C or `SIGTERM`; on Windows also Ctrl+Break,
closing the console window, logoff, or system shutdown) drains the queue first.

## Completeness: TailFlow never hides missing history

Every `/api/v1/events` response includes a `completeness` object:

```json
"completeness": {
  "complete": false,
  "reasons": ["loss_in_range"],
  "gaps": [{"reason": "queue_full", "count": 412, "from_seq": 9120, "to_seq": 9120}],
  "retention_truncated": false,
  "history_starts_at": "2026-10-24T09:12:03Z",
  "evicted_through_seq": 0
}
```

`complete` is `false` whenever loss, a deletion, or an index hole overlaps the
range the query scanned. Possible loss reasons:

| Reason | Meaning |
|---|---|
| `ingest_lag` | The live bus overflowed; the dashboard and legacy endpoints lost records too |
| `queue_full` | The storage queue overflowed; the live view was unaffected |
| `storage_error` | A batch could not be committed (disk full, I/O error) after retries |
| `unclean_shutdown` | A previous daemon run was killed; the number of lost events is unknown (`count: null`) |
| `writer_stopped` | Events arrived after storage shut down |
| `session_deleted` | You deleted a session's events on purpose |

Loss is also visible outside the v1 API:

- `/health`, `/api/sources`, `/api/query`, `/api/errors`, and `/api/wait` carry
  `ingest_dropped` (records lost at ingestion in this run);
- an `[tailflow] N log records were dropped` warning appears in the live stream;
- SSE subscribers that fall behind receive a named `gap` event; and
- `tailflow-mcp` / `tailflow-logs` print a warning and no longer say a wait
  condition "did not occur" after loss.

Expiry is different from loss. When retention removes old events,
`retention_truncated` is `true` and `history_starts_at` says where retained
history begins; `complete` stays `true` because nothing was lost unexpectedly.
A cursor that points below the retention horizon is reported with reason
`cursor_behind_retention`. Periods when no daemon was running are not
recorded as gaps — nothing was collecting.

## Retention

- **Age:** events whose receive time is older than `retention_days` are deleted,
  oldest first. Expiry is deterministic: it always removes a prefix of history.
- **Payload age:** optional payload bodies are cleared after
  `payload_retention_days`; the events themselves remain.
- **Size cap:** when live database pages exceed `max_size_mb`, the oldest events
  are evicted until usage falls to 90% of the cap.
- Cleanup runs at startup and every `cleanup_interval_secs` (default 300) in
  short transactions, and returns free pages to the filesystem gradually.
- A cleanup cycle that removed events ends with a bounded merge (about 64
  pages) of the full-text index segments that consist mostly of deleted
  events' entries — and so does every 20th pass of a long cleanup backlog — so
  those entries do not accumulate and take the size cap away from history.

The size cap applies to live pages. The file on disk can briefly exceed it by
the WAL (capped at 64 MB after checkpoints) and by free pages not yet returned;
after a large eviction the file shrinks gradually (about 4 MB per pass).

Cleanup passes aim for 250 ms. Clearing a large backlog — for example after
lowering `max_size_mb` on a multi-gigabyte database — can make individual passes
take a second or two; persistence waits behind them, live logs do not.

## Resource cost

Measured on a 2018 4-core laptop: idle CPU stays at 0 %, and persistence adds
roughly 35 µs of CPU per stored event (about 2 % of one core at 100 events/s).
Each event takes about 400 bytes on disk including indexes, so the 2 GB default
holds roughly 5 million events. Setting `full_text_search = false` cuts the
write cost by about a third; `--no-storage` removes it.

## Privacy

```toml
[privacy]
capture = "metadata"          # metadata (default) | redacted | full
capture_enabled = true        # false = record nothing durable
disabled_rules = []           # turn off built-in rules by name
excluded_event_types = []     # e.g. ["runtime.log"] or ["git.*"]
excluded_agents = []
allowed_project_dirs = []     # prepared for future file/agent collectors
excluded_file_globs = []      # e.g. ["**/.env", "**/secrets/**"]

[[privacy.redaction_rules]]
name = "internal-token"
pattern = 'itk_(?P<secret>[a-z0-9]{32})'
```

| Mode | Log lines and attributes | Optional payload bodies |
|---|---|---|
| `metadata` | Redacted | Not stored |
| `redacted` | Redacted | Stored after redaction |
| `full` | Verbatim | Verbatim — stores secrets; explicit opt-in |

Redaction runs **once, at ingestion**, before a line reaches the live
dashboard, SSE, `/api/query`, MCP, or the database. Built-in rules cover
provider API keys (AWS, GitHub, Slack, Stripe, OpenAI, Anthropic, Google),
JWTs, `Authorization` headers, bearer tokens, URL passwords, PEM private keys
(including multi-line keys), and `password=` / `*_TOKEN=` / `api_key:` style
assignments. Matches are replaced with `[REDACTED:<rule>]`.

> **This changes the live output** of earlier versions: a line such as
> `password=hunter2` now appears as `password=[REDACTED:secret_assignment]` in
> every interface. Set `capture = "full"` to restore verbatim output. The
> daemon's own `--grep`/`--source` startup filter still sees the original line;
> every query filter (`grep`, `q`) sees the redacted text.

Redaction is a heuristic. It removes common, high-confidence credential shapes
and cannot recognise every secret — an opaque random string in free text looks
like any other identifier. Do not rely on it as the only protection for
sensitive logs.

`system.events.dropped` cannot be excluded, so loss stays visible regardless of
policy. Exclusions are counted (`events_excluded`) and are not reported as loss.

## Inspecting health

```bash
tailflow-daemon storage status         # size, events, oldest/newest, gaps
tailflow-daemon storage status --json
tailflow-daemon storage check          # full integrity check, read-only
curl -s localhost:7878/api/v1/storage/status
curl -s localhost:7878/api/v1/collectors/status
```

The dashboard's **diagnostics** button shows the same information.

## Clearing history

```bash
tailflow-daemon storage purge --all --yes
tailflow-daemon storage purge --before 7d --yes      # received more than 7 days ago
tailflow-daemon storage purge --session SESSION_ID --yes
```

Without `--yes` the command only reports what it would delete. Purging is safe
while a daemon is running. There is no HTTP endpoint that deletes data.

## When storage is unavailable

If the database cannot be opened — not a SQLite file, created by a newer
TailFlow, another application's database, read-only, or an invalid path — the
daemon prints the reason, **leaves the file untouched**, and keeps running with
memory-only history. `/api/v1/storage/status` reports `state: "unavailable"`
and the v1 event endpoints return `503`. TailFlow never deletes or recreates a
database because it failed to open.

If writes start failing while running (for example, the disk fills), the writer
retries, then counts the batch as lost, reports `state: "degraded"`, and records
the gap as soon as writes succeed again. If the daemon has to stop before the
gap can be written, the run is left marked as unclean, so the next start
records the loss instead.

After an unclean shutdown the daemon runs `PRAGMA quick_check` in the
background (it takes seconds per gigabyte) and reports the outcome under
`integrity_check` in `/api/v1/storage/status`; startup does not wait for it.

`health.last_error` in the status response is local diagnostic text and can
contain SQLite messages and file paths.

## HTTP API (v1)

All endpoints are read-only `GET`s on the loopback listener.

| Endpoint | Returns |
|---|---|
| `/api/v1/events` | A page of events with `next_cursor`, `has_more`, and `completeness` |
| `/api/v1/events/{seq}` | One event; `404` if it never existed, `410` if retention removed it |
| `/api/v1/storage/status` | Database size, counts, horizon, writer health, ingestion counters |
| `/api/v1/storage/retention` | Retention settings and the retained horizon |
| `/api/v1/collectors/status` | Per-collector status and event counts, loss summary |
| `/api/v1/schema` | Event-type catalog and schema version |
| `/api/v1/privacy` | Capture mode and active redaction rules |

`/api/v1/events` parameters (lists are comma-separated):

| Parameter | Meaning |
|---|---|
| `type` | Event types, or a family such as `file.*` |
| `source`, `session`, `agent`, `project` | Exact values |
| `severity` | Minimum: `trace`, `debug`, `info`, `warn`, `error` |
| `status` | `started`, `succeeded`, `failed`, `cancelled` |
| `path` / `path_prefix` | Glob (`**` crosses `/`, `*` does not) or prefix |
| `command` | Substring of `attributes.command` |
| `q` | Full-text terms, all required; `term*` matches a prefix |
| `since`, `until` | RFC 3339 or relative (`30s`, `5m`, `2h`, `7d`) |
| `after` / `before` | Cursor: pass `next_cursor` back (`after` for `order=asc`, `before` for `desc`) |
| `order` | `asc` (default) or `desc` |
| `limit` | `1..=1000`, default 100 (`1..=100` with `include_payload`) |
| `include_payload` | Include payload bodies (if the capture mode stored them) |

A page also stops early, with `has_more: true`, once it holds about 8 MiB of
text, so no single response can grow without bound.

Results are ordered by `seq`, the order TailFlow received events, so pages never
skip or repeat events with identical timestamps, and `seq` keeps increasing
across restarts. Unknown parameters and invalid values return `400`. One
request scans a bounded range of history; if it stops early it sets
`has_more: true` and `scan_budget_exhausted: true` and returns a cursor that
always advances — keep paging.

```bash
curl -s 'localhost:7878/api/v1/events?type=runtime.error&since=24h&order=desc&limit=20'
curl -s 'localhost:7878/api/v1/events?q=connection%20refused&source=api'
```

## Moving from memory-only versions

No migration is needed. On first start the daemon creates the database and
begins recording; earlier history did not exist on disk and is not
reconstructed. Every legacy endpoint (`/api/records`, `/api/query`,
`/api/errors`, `/api/sources`, `/api/wait`, `/events`, `/health`) keeps its
in-memory ring, cursor semantics, and response fields; new fields are additive.
To keep the previous behavior exactly, run with `--no-storage` and
`capture = "full"`.
