import { useEffect, useState } from 'preact/hooks'
import { isSessionChange } from '../lib/liveFilters.ts'
import { api } from '../lib/api.ts'
import { useAsync, navigate, useNow } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { between, duration, projectName, relative, shortId } from '../lib/format.ts'
import type { SessionView } from '../lib/types.ts'
import {
  Agent, Card, CopyButton, Empty, ErrorState, HistoryBadge, Loading, Num, SessionStatusBadge, Time,
} from '../components/ui'
import { Icon } from '../components/icons'

const PAGE = 50

export function Sessions({ params }: { params: URLSearchParams }) {
  const p = toRecord(params)
  const [text, setText] = useState(p.q ?? '')
  useEffect(() => setText(p.q ?? ''), [p.q])
  const set = (changes: Record<string, string | null>) => navigate(href('sessions', withParams(params, changes)), true)
  const filters = { agent: p.agent, status: p.status, since: p.since, q: p.q, project: p.project }

  // Later pages accumulate until the filters change; each "Load more"
  // follows the cursor of the last page loaded. The first page polls only
  // while it is the only page: refreshing it under pages loaded from the old
  // cursor would drop the sessions pushed across the page boundary.
  const key = JSON.stringify(filters)
  const [extra, setExtra] = useState<{ key: string; sessions: SessionView[]; cursor: string | null; hasMore: boolean }>(
    { key, sessions: [], cursor: null, hasMore: false })
  const olderLoaded = extra.key === key && extra.sessions.length > 0
  // Live updates (like polling) only while the list is one page.
  const first = useAsync(s => api.sessions({ ...filters, limit: PAGE }, s), [key],
    olderLoaded ? undefined : 15_000, olderLoaded ? undefined : isSessionChange)
  const [loadingMore, setLoadingMore] = useState(false)
  const [moreError, setMoreError] = useState<unknown>(null)
  useNow(30_000)

  const ext = extra.key === key ? extra : { key, sessions: [], cursor: null, hasMore: false }
  const started = ext.sessions.length > 0
  const nextCursor = started ? ext.cursor : first.data?.next_cursor ?? null
  const hasMore = started ? ext.hasMore : !!first.data?.has_more
  const seen = new Set<string>()
  const sessions = [...(first.data?.sessions ?? []), ...ext.sessions].filter(x => {
    if (seen.has(x.session_id)) return false
    seen.add(x.session_id)
    return true
  })
  const loadMore = async () => {
    if (!nextCursor) return
    setLoadingMore(true)
    setMoreError(null)
    try {
      const page = await api.sessions({ ...filters, limit: PAGE, cursor: nextCursor })
      setExtra({ key, sessions: [...ext.sessions, ...page.sessions], cursor: page.next_cursor, hasMore: page.has_more })
    } catch (e) {
      setMoreError(e)
    } finally {
      setLoadingMore(false)
    }
  }

  const anyFilter = Object.values(filters).some(Boolean)
  return (
    <div class="page">
      <div class="page-head">
        <div>
          <h2>Sessions</h2>
          <p>Every coding-agent session TailFlow has captured, most recent activity first.</p>
        </div>
        <CopyButton text={location.href} label="Copy filter URL" />
      </div>
      <form class="toolbar" role="search" onSubmit={e => { e.preventDefault(); set({ q: text.trim() || null }) }}>
        <input class="input grow" type="search" value={text} placeholder="Search id, project, directory or branch…"
          aria-label="Search sessions" onInput={e => setText((e.target as HTMLInputElement).value)} />
        <select class="select" aria-label="Agent" value={p.agent ?? ''} onChange={e => set({ agent: (e.target as HTMLSelectElement).value || null })}>
          <option value="">All agents</option>
          <option value="claude-code">Claude Code</option>
          <option value="codex">Codex</option>
        </select>
        <select class="select" aria-label="Status" value={p.status ?? ''} onChange={e => set({ status: (e.target as HTMLSelectElement).value || null })}>
          <option value="">Any status</option>
          <option value="active">Active</option>
          <option value="ended">Ended</option>
          <option value="unterminated">No end observed</option>
        </select>
        <select class="select" aria-label="Time range" value={p.since ?? ''} onChange={e => set({ since: (e.target as HTMLSelectElement).value || null })}>
          <option value="">Any time</option>
          <option value="1h">Last hour</option>
          <option value="24h">Last 24 h</option>
          <option value="7d">Last 7 days</option>
          <option value="30d">Last 30 days</option>
        </select>
        <button class="btn" type="submit"><Icon name="search" size="sm" />Search</button>
      </form>
      {p.project && (
        <div class="chips">
          <span class="chip"><span>project: {p.project}</span><button aria-label="Remove project filter" onClick={() => set({ project: null })}><Icon name="x" size="sm" /></button></span>
        </div>
      )}
      <Card flush>
        {first.error ? <ErrorState error={first.error} onRetry={first.reload} />
          : !first.data ? <Loading rows={6} />
          : sessions.length === 0 && hasMore ? (
            <Empty icon="filter" title="None in the most recent sessions scanned">The status filter is applied after reading; older sessions have not been checked yet. Use Load more to keep looking.</Empty>
          ) : sessions.length === 0 ? (
            anyFilter
              ? <Empty icon="filter" title="No sessions match these filters">Try a wider time range or clear the search.</Empty>
              : <Empty icon="sessions" title="No agent sessions captured yet">
                  Run <code>tailflow agent install claude</code> or <code>tailflow agent install codex</code>, then start the agent in a project. See Integrations for status.
                </Empty>
          ) : (
            <div class="table-wrap">
              <table class="table">
                <thead><tr>
                  <th>Agent</th><th>Project</th><th>Status</th><th class="hide-sm">Started</th><th>Last activity</th>
                  <th class="hide-sm num">Duration</th><th class="num">Tools</th><th class="num hide-sm">Commands</th>
                  <th class="num hide-sm">Files</th><th class="num">Failures</th><th class="hide-sm">History</th>
                </tr></thead>
                <tbody>
                  {sessions.map(s => <SessionRow key={s.session_id} s={s} />)}
                </tbody>
              </table>
            </div>
          )}
        {first.data && hasMore && (
          <div style={{ padding: '12px', textAlign: 'center' }}>
            <button class="btn" onClick={loadMore} disabled={loadingMore}>{loadingMore ? 'Loading…' : 'Load more'}</button>
            {moreError ? <p class="note warn" role="alert">{(moreError as Error).message}</p> : null}
          </div>
        )}
      </Card>
      <p class="note">
        <strong>Status</strong> is what TailFlow observed: <em>Active</em> = activity in the last 30 min with no end; <em>No end observed</em> = idle for 30 min with no end event (crashed, killed or just idle — TailFlow cannot tell).
        Durations are the observed span from first to last event, not proof the agent was working throughout.
      </p>
    </div>
  )
}

function SessionRow({ s }: { s: SessionView }) {
  const go = () => navigate(href('session', {}, s.session_id))
  const span = between(s.first_event_at, s.status === 'ended' ? (s.ended_at ?? s.last_event_at) : s.last_event_at)
  const fails = s.counts.failed_operations
  return (
    <tr class="clickable" tabIndex={0} onClick={go} onKeyDown={e => { if (e.key === 'Enter') go() }}>
      <td><Agent name={s.agent_name} /></td>
      <td title={s.project_id ?? s.working_directory ?? ''}>
        <div class="cell-main">
          {projectName(s.project_id ?? s.working_directory)}
          <span class="sub">{shortId(s.session_id)}{s.git.branch ? ` · ${s.git.branch}` : ''}{s.model ? ` · ${s.model}` : ''}</span>
        </div>
      </td>
      <td><SessionStatusBadge status={s.status} /></td>
      <td class="hide-sm dim">{s.started_at ? <Time at={s.started_at} /> : <span title="Start not observed">— <Time at={s.first_event_at} /></span>}</td>
      <td class="dim">{relative(s.last_event_at)}</td>
      <td class="hide-sm num">{duration(span)}</td>
      <Num n={s.counts.tool_calls} />
      <td class="hide-sm num">{s.counts.commands.toLocaleString('en-US')}</td>
      <td class="hide-sm num">{s.counts.file_events.toLocaleString('en-US')}</td>
      <Num n={fails} bad />
      <td class="hide-sm"><HistoryBadge history={s.coverage.history} /></td>
    </tr>
  )
}
