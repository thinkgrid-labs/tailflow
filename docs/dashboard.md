# Dashboard

> **Next release.** The agent observability dashboard is merged on `dev` and
> ships in the next version. v0.3.3 has the earlier live-log view only.

The daemon serves a local dashboard at `http://127.0.0.1:7878` (or the port
you pass with `--port`). It shows what your coding agents did, what your
running stack logged, and how the two line up in time. It talks only to the
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
  said so. TailFlow does not watch the file system, so it did not confirm the
  change, and it does not see edits made by shell commands.
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
- **Nearby events are not causes.** The event inspector lists runtime warnings
  and errors within ±60 seconds of an agent operation, and agent activity near
  a log line. They are shown because they are close in time. The dashboard
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

- Agent views refresh every 10–30 seconds, and only while the tab is visible.
  A hidden tab makes no requests and does no rendering work.
- Every count comes from SQLite indexes on the daemon. Each query runs under a
  3-second budget and returns a bounded response. The browser never downloads
  the database to filter it.
- The live log view renders about 60 rows whatever the buffer holds. In a
  headless-Chrome benchmark streaming 1,000 lines/s into a full 5,000-line
  view it used ~19 % of the browser's main thread at a steady 60 fps (the
  previous view: ~99 % at 6–8 fps). Expanding large JSON rows costs more.
- An open dashboard page added 0.07–0.17 % of one core to the daemon in the
  same tests.
- The whole UI is one JavaScript file (~46 KB gzip) and one stylesheet,
  embedded in the daemon binary. It uses no web fonts and no third-party
  scripts.

## Security

- The page is served with a strict Content Security Policy: only the daemon's
  own scripts and styles, no inline script, and it cannot be framed by another
  site.
- Log lines and agent metadata are untrusted text and are always rendered as
  text, never as HTML.
- The dashboard cannot change anything. Installing or removing hooks, purging
  history and every other change goes through the `tailflow` CLI, which is
  protected by file-system permissions.
- The daemon accepts browser requests only from loopback origins. Do not
  expose it to a network: it has no authentication.

## API used by the dashboard

All `GET` and read-only. The dashboard uses the same public endpoints that
agents and scripts can call:

| Endpoint | Used for |
|---|---|
| `GET /api/v1/stats?since=24h&buckets=48` | Overview counts and histogram, from indexes |
| `GET /api/v1/sessions?agent=&status=&q=&since=&until=&cursor=` | Session list with cursor pagination and text search |
| `GET /api/v1/sessions/{id}` | Session summary and coverage |
| `GET /api/v1/events?…&program=` | Activity, errors, commands, timeline windows |
| `GET /api/v1/files?since=7d&multi_agent=true` | Per-path reported file activity |
| `GET /api/v1/agents/status` | Hook installation (read-only check), capabilities, spool and ingest counters |
| `GET /events` (SSE), `GET /api/records?after=` | Live log stream and reconnect catch-up |

See [durable history](storage.md) for the full `/api/v1/events` reference.
