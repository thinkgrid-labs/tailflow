# Dashboard

> **Since v0.4.0.** v0.3.3 and earlier have the live-log view only.

The daemon serves a local dashboard at `http://127.0.0.1:7878` (or the port
you pass with `--port`). Sign in once per browser with the one-time link the
daemon prints at startup, or run `tailflow-daemon dashboard` to open the
browser with a fresh link (see [security](security.md)). The session
survives a daemon restart on the same port within 15 minutes. It shows what
your coding agents did, what your running stack logged, and how the two line
up in time. It talks only to the
daemon that served it, through read-only endpoints, and loads nothing from
the internet.

| Page | Use it to |
|---|---|
| **Overview** | See active sessions, tool calls, commands, reported file changes, failures and ingested volume for a time range, plus per-agent status |
| **Sessions** | Browse and filter every captured Claude Code and Codex session, then open one |
| **Activity** | Search all durable history with filters and full-text search; open any event |
| **Timeline** | See agents side by side with runtime warnings and errors, zoomable from minutes to days |
| **Logs** | Follow live runtime output (the newest 5,000 lines), filter it, expand JSON |
| **Errors** | Reported failures, runtime errors and TailFlow loss markers, newest first |
| **Files** | File operations agents reported, grouped by path, project or agent |
| **Commands** | Shell commands agents reported, with exit status and duration when known |
| **Integrations** | Hook installation state, what each integration can see, spool backlog, storage health, completeness and privacy settings |
| **Settings** | Theme (system, dark, light), local or UTC time, density, log wrapping |

Every view keeps its filters in the URL. **Copy filter URL** and **Copy link**
(on an event) give you a link that reopens the same view on this machine.

## Reading what you see

The dashboard is built to never claim more than the data supports.

- **Agent-reported, not verified.** Every agent event comes from the agent's
  own hook. A file shown as *Reported modified* means the agent's edit tool
  said so; hooks do not report edits made by shell commands. Only with
  `[observe]` set does TailFlow also watch the project's files, and then
  **Files → Verified on disk** shows whether a change was also observed
  ([below](#verified-changes-and-git-evidence)). An observation still does
  not say who made the change.
- **Outcome not reported.** Codex has no failure hook and its tool responses
  carry no documented exit code. Its tool calls show *Outcome not reported*,
  never *Succeeded*. A Claude Code shell command that succeeded shows
  *Reported success*: Claude Code reports exit codes only on failure.
- **Session status** is what TailFlow observed:
  - *Active*: activity in the last 30 minutes and no end event.
  - *Ended*: an end event was observed.
  - *No end observed*: idle for 30 minutes without an end event. The agent
    may have crashed, been killed or just be idle; TailFlow cannot tell which.
- **History coverage** is shown on every session. *Complete* means start and
  end were observed, nothing was lost and nothing expired, within what that
  integration can see. *Partial* and *Unknown* list the reason.
- **Loss and limits are different things.** *Recorded event loss* means
  TailFlow dropped events, for example because the agent spool was full. It is
  counted and listed. *Integration limits* are activities a hook cannot report
  at all, such as file reads for Codex. Zero for a limited activity means
  "not seen", not "did not happen".
- **Git HEAD.** When HEAD differs between a session's observed start and end,
  the session page says so. That is an observation of two boundaries, not proof
  that the agent made the commit.
- **Nearby events are not causes.** The event inspector's **Related
  evidence** lists runtime warnings and errors within ±30 seconds of an agent
  operation, and agent activity near a log line. They are shown because they are close in time. The dashboard
  never labels one as caused by the other.

## Timeline

Each agent gets a lane, and a runtime lane underneath shows warnings and
errors. Session bars cover a session's **observed** span, from its first event
to its last.

- A bar is drawn only when the agent reported both a start and an end, or
  reported a duration. Everything else is a point (◆) at its own timestamp.
- A start whose end is outside the window has an amber outline. It is never
  stretched to "now".
- Drag to pan. Shift + drag selects a range to zoom into. Ctrl/⌘ + scroll
  zooms. With the timeline focused, `+` / `−` zoom and `←` / `→` pan.
- One window loads at most 3,000 agent events and 500 runtime markers. If a
  window holds more, the page says so; zoom in to see everything.

## Live logs

The log view keeps the newest 5,000 lines in the browser and renders only the
rows on screen, so a full buffer costs about the same as a near-empty one.

- Scrolling up, or pressing **Pause**, holds the view still. New lines are
  counted and applied when you resume. If more than 5,000 arrive while paused,
  the oldest of them are only in history, and the page says how many.
- Lines are real text, so you can select and copy them. Because off-screen
  rows are not rendered, the browser's find-in-page and multi-screen
  selections only cover what is on screen. Use the filter box, or **Copy
  lines**, which copies every line that matches the current filters.
- **Search history** opens Activity for anything older than the live window.
- If the browser falls behind the stream, or the daemon restarts, a banner
  says so and the view catches up from the daemon's buffer.

## Keyboard

| Key | Action |
|---|---|
| `/` | Focus search |
| `j` / `k` | Next / previous event in a list |
| `Enter` | Open the focused row |
| `Esc` | Close the event inspector |
| `+` / `−`, `←` / `→` | Zoom and pan the focused timeline |

The dashboard has a skip link, labels on every control, visible focus, and
status badges that always carry text, not just color. It respects your
operating system's reduced-motion setting.

## Cost

The dashboard is meant to be left open all day.

- Agent views update from a live stream, but they do not insert rows one
  by one. The daemon streams committed agent events to the dashboard (one
  connection per tab, `/api/v1/stream/agent`); when an event concerns a
  page, the page re-runs its own bounded query, at most once a second
  (every 3 seconds for the timeline). The **Agents** pill in the top bar
  shows the stream's state.
- Some views do not follow the stream: **Integrations** refreshes every 10
  seconds, and the Overview's agent cards and the observation status on
  **Files** every 30 seconds. The **Timeline** follows it only while the
  window ends at "now". **Sessions**, **Activity** and **Commands** stop
  updating once you load older pages, so the rows you are reading do not
  move.
- When the pill reads **Polling** (the stream is not available), most views
  refresh every 10–30 seconds instead; Activity search does not refresh on
  its own.
- A tab hidden for a minute closes the stream and makes no requests. When
  it is shown again it reconnects from the last event it received, and the
  daemon catches it up from durable history.
- Every count comes from SQLite indexes on the daemon. Each query runs under a
  3-second budget and returns a bounded response. The browser never downloads
  the database to filter it.
- The live log view renders about 60 rows whatever the buffer holds.
  Expanding large JSON rows costs more.
- While a tab is connected, the daemon checks for new commits every 250 ms,
  so it wakes about ten times a second instead of staying idle.
- The UI is about 56 KB of JavaScript (gzip): a main file of about 34 KB,
  plus per-page chunks that load on first visit, and one stylesheet. All of
  it is embedded in the daemon binary. It uses no web fonts and no
  third-party scripts.

## Security

- The page is served with a strict Content Security Policy: only the daemon's
  own scripts and styles, no inline script, and it cannot be framed by another
  site.
- Log lines and agent metadata are untrusted text and are always rendered as
  text, never as HTML.
- The dashboard cannot change anything. Installing or removing hooks, purging
  history and every other change goes through the `tailflow` CLI, which is
  protected by file-system permissions.
- Every data endpoint requires a credential. The dashboard holds an
  `HttpOnly; SameSite=Strict` session cookie that other localhost ports
  cannot use, and pages on other origins cannot read the API
  ([security](security.md)). Sign out in **Settings → Access**.
- The daemon serves plain HTTP on loopback. Do not expose it to a network.

## Verified changes and Git evidence

With `[observe]` (or `--observe`) set, three views use independent evidence
([evidence](evidence.md)):

- **Files → Verified on disk** lists each agent-reported change with its
  evidence status, plus unreported observed changes, with counts per status,
  the observation window, observed roots, and a warning when the watcher lost
  notifications.
- **Session → Git changes** compares the worktree before and after the
  session, gives each changed path its evidence (reported by this session,
  by another, by several, or unreported), lists pre-existing changes
  separately, and names other sessions active in the repository.
- **Integrations → File & Git observation** shows each observed directory's
  state, backend, counts, loss and coverage, and each repository's snapshots.

Nothing in these views says who made a change; a match is an association.

## Outcomes and related evidence

- **Commands → Tests, builds & checks** classifies commands (build, test,
  lint, type check, format, dependencies), shows the outcome only as the
  agent reported it, and lists runtime errors during or shortly after each
  one as nearby. Counts per kind link to filtered views.
- **Session → Tools, files & failures** starts with the session's tests,
  builds and checks.
- **The event inspector** shows **Related evidence** in five tiers (direct
  identifiers, independent observation, shared context, temporal
  proximity, inference), each item with its reason.
- **Event lists** mark filesystem and Git observations with **on disk**
  and **git** instead of an agent name.

## API used by the dashboard

The dashboard reads only through `GET` endpoints that agents and scripts can
call too. Its only other request is `POST /api/v1/auth/logout` for
**Settings → Access → Sign out**.

| Endpoint | Used for |
|---|---|
| `GET /api/v1/stats?since=24h&buckets=48` | Overview counts and histogram, from indexes |
| `GET /api/v1/sessions?agent=&status=&q=&since=&until=&cursor=` | Session list with cursor pagination and text search |
| `GET /api/v1/sessions/{id}` | Session summary and coverage |
| `GET /api/v1/events?…&program=` | Activity, errors, commands, timeline windows |
| `GET /api/v1/files?since=7d&multi_agent=true` | Per-path reported file activity |
| `GET /api/v1/agents/status` | Hook installation (read-only check), capabilities, spool and ingest counters |
| `GET /api/v1/stream/agent` (SSE) | Live agent events that trigger the views' reloads |
| `GET /api/v1/events/{seq}`, `GET /api/v1/events/{seq}/related` | The event inspector and its related evidence |
| `GET /api/v1/evidence/files`, `GET /api/v1/sessions/{id}/changes`, `GET /api/v1/observe/status` | Verified on disk, Git changes, observation status |
| `GET /api/v1/outcomes` | Tests, builds & checks |
| `GET /api/v1/storage/status`, `GET /api/v1/collectors/status`, `GET /api/v1/privacy`, `GET /health` | Integrations and Settings diagnostics |
| `GET /api/v1/auth/session` | Whether this browser is signed in |
| `GET /events` (SSE), `GET /api/records?after=` | Live log stream and reconnect catch-up |

See [durable history](storage.md) for the full `/api/v1/events` reference.
