import { useEffect, useState } from 'preact/hooks'
import { api, ApiError } from '../lib/api.ts'
import { useAsync, navigate, useSettings } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { between, count, dateTime, duration, projectName, relativeTo } from '../lib/format.ts'
import { ACTIVITY_NAME, COMPLETENESS_REASON, COVERAGE_REASON, GAP_REASON, HISTORY_TEXT, LEVEL_TEXT } from '../lib/events.ts'
import type { SessionChanges, SessionDetail, StoredEvent } from '../lib/types.ts'
import { CHANGE_EVIDENCE, CHANGE_KIND } from '../lib/evidence.ts'
import { OutcomeTable } from './Commands'
import {
  Agent, Badge, Callout, Card, CopyButton, Empty, ErrorState, HistoryBadge, Loading, Segmented, SessionStatusBadge, Stat,
} from '../components/ui'
import { EventList, moveSelection } from '../components/EventList'
import { Inspector } from '../components/Inspector'
import { Icon } from '../components/icons'

const PAGE = 500
/** Events kept in the browser for one session view; beyond this, narrow with Activity filters. */
const MAX_LOADED = 20_000

type Tab = 'events' | 'summary' | 'changes' | 'coverage'

export function SessionDetailPage({ id, params }: { id: string; params: URLSearchParams }) {
  const p = toRecord(params)
  const tab = (p.tab as Tab) || 'events'
  const selected = p.event ? Number(p.event) : null
  const set = (c: Record<string, string | number | null>) => navigate(href('session', withParams(params, c), id), true)
  const detail = useAsync(s => api.session(id, s), [id], 15_000, e => e.session_id === id)
  const d = detail.data

  if (detail.error) {
    const notFound = detail.error instanceof ApiError && detail.error.status === 404
    return (
      <div class="page">
        <a href={href('sessions')} class="btn sm ghost" style={{ alignSelf: 'flex-start' }}><Icon name="left" size="sm" />Sessions</a>
        <Card>{notFound
          ? <Empty icon="sessions" title="Session not found">No session <code>{id}</code>. It may have expired under retention, or the id is mistyped.</Empty>
          : <ErrorState error={detail.error} onRetry={detail.reload} />}</Card>
      </div>
    )
  }
  if (!d) return <div class="page"><Card><Loading rows={6} /></Card></div>

  return (
    <div class="page" style={{ maxWidth: 'none' }}>
      <a href={href('sessions')} class="btn sm ghost" style={{ alignSelf: 'flex-start' }}><Icon name="left" size="sm" />Sessions</a>
      <Header d={d} />
      <div class="toolbar">
        <Segmented label="Session view" value={tab} onChange={v => set({ tab: v === 'events' ? null : v })} options={[
          { value: 'events', label: `Events (${count(d.counts.events)})` },
          { value: 'summary', label: 'Tools, files & failures' },
          { value: 'changes', label: 'Git changes' },
          { value: 'coverage', label: 'Coverage' },
        ]} />
        <div class="grow" />
        <a class="btn sm" href={href('timeline', {
          from: Date.parse(d.first_event_at ?? '') - 60_000 || undefined,
          to: Date.parse(d.last_event_at ?? '') + 60_000 || undefined,
          agent: d.agent_name,
        })}><Icon name="timeline" size="sm" />Open in timeline</a>
        <a class="btn sm" href={href('activity', { session: d.session_id })}><Icon name="activity" size="sm" />Search this session</a>
      </div>
      {tab === 'events' && <SessionEvents id={id} selected={selected} onSelect={seq => set({ event: seq })} onClose={() => set({ event: null })} />}
      {tab === 'summary' && <Summary d={d} />}
      {tab === 'changes' && <Changes id={id} />}
      {tab === 'coverage' && <Coverage d={d} />}
    </div>
  )
}

function Header({ d }: { d: SessionDetail }) {
  const { settings } = useSettings()
  const end = d.status === 'ended' ? d.ended_at : null
  const span = between(d.first_event_at, end ?? d.last_event_at)
  const g = d.git
  return (
    <>
      <div class="page-head">
        <div>
          <div class="toolbar" style={{ marginBottom: '4px' }}>
            <Agent name={d.agent_name} />
            <SessionStatusBadge status={d.status} />
            <HistoryBadge history={d.coverage.history} />
            {d.restarts > 0 && <Badge tone="outline" title="Additional starts observed (resume, clear, compact)">{d.restarts} restart{d.restarts === 1 ? '' : 's'}</Badge>}
          </div>
          <h2 title={d.project_id ?? ''}>{projectName(d.project_id ?? d.working_directory)}</h2>
          <p class="mono">{d.session_id}</p>
        </div>
        <CopyButton text={d.session_id} label="Copy session id" />
      </div>
      {d.coverage.history !== 'complete' && (
        <Callout tone="warn">
          History is <strong>{HISTORY_TEXT[d.coverage.history].label.toLowerCase()}</strong>: {d.coverage.reasons.map(r => COVERAGE_REASON[r] ?? r).join(' ')}
        </Callout>
      )}
      <Card>
        <dl class="kv">
          <dt>Started</dt><dd>{d.started_at ? dateTime(d.started_at, settings.tz) : <span class="dim">not observed — first event {dateTime(d.first_event_at, settings.tz)}</span>}{d.start_source && <span class="dim"> ({d.start_source})</span>}</dd>
          <dt>Ended</dt><dd>{end ? <>{dateTime(end, settings.tz)}{d.end_reason && <span class="dim"> ({d.end_reason})</span>}</> : <span class="dim">{d.status === 'active' ? 'in progress' : 'no end observed'} — last event {dateTime(d.last_event_at, settings.tz)}</span>}</dd>
          <dt>Observed span</dt><dd>{duration(span)} <span class="dim">first to last event</span></dd>
          {d.model && <><dt>Model</dt><dd>{d.model}</dd></>}
          <dt>Directory</dt><dd class="mono">{d.working_directory ?? '—'}</dd>
          {(g.root || g.branch) && <>
            <dt>Git</dt>
            <dd>
              <span class="toolbar">
                {g.branch && <Badge tone="outline" icon="branch">{g.branch}</Badge>}
                {g.worktree && <Badge tone="outline">worktree {g.worktree}</Badge>}
                <span class="mono dim">{g.root}</span>
              </span>
            </dd>
            <dt>HEAD</dt>
            <dd>
              <span class="mono">{g.head_at_start?.slice(0, 10) ?? '?'}</span> at start → <span class="mono">{g.head_at_end?.slice(0, 10) ?? (d.status === 'ended' ? '?' : 'not ended')}</span> at end
              {g.inferred_head_changed && <div class="note" style={{ marginTop: '4px' }}>HEAD differs between the observed start and end. This is an observation of two boundaries, not proof the agent made the commit — it may have been made in another terminal.</div>}
            </dd>
          </>}
        </dl>
      </Card>
      <div class="grid stats">
        <Stat label="Tool calls" icon="tool" value={count(d.counts.tool_calls)} foot={d.counts.tool_failures ? `${d.counts.tool_failures} failed` : ' '} />
        <Stat label="Commands" icon="commands" value={count(d.counts.commands)} foot={d.counts.command_failures ? `${d.counts.command_failures} reported failing` : ' '} />
        <Stat label="Reported file events" icon="files" value={count(d.counts.file_events)} foot="agent-reported" />
        <Stat label="Failed operations" icon="errors" value={count(d.counts.failed_operations)} tone={d.counts.failed_operations ? 'error' : undefined}
          foot="a failing Bash call counts once" />
        <Stat label="Subagents" icon="agent" value={count(d.counts.subagents)} foot={`${count(d.counts.turns)} turn${d.counts.turns === 1 ? '' : 's'}`} />
        <Stat label="Tokens" icon="cpu" value={d.counts.tokens_total === null ? '—' : count(d.counts.tokens_total)} foot={d.counts.tokens_total === null ? 'not reported by this agent' : 'as reported'} />
      </div>
    </>
  )
}

function SessionEvents({ id, selected, onSelect, onClose }: { id: string; selected: number | null; onSelect: (seq: number) => void; onClose: () => void }) {
  const [events, setEvents] = useState<StoredEvent[]>([])
  const [cursor, setCursor] = useState<number | null>(null)
  const [hasMore, setHasMore] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const [loading, setLoading] = useState(true)
  const [complete, setComplete] = useState<{ complete: boolean; reasons: string[] } | null>(null)

  const load = async (after: number | null, reset: boolean) => {
    setLoading(true)
    try {
      const page = await api.events({ session: id, order: 'asc', limit: PAGE, after })
      setEvents(prev => {
        const next = reset ? page.events : [...prev, ...page.events]
        return next.length > MAX_LOADED ? next.slice(0, MAX_LOADED) : next
      })
      setCursor(page.next_cursor)
      setHasMore(page.has_more)
      setComplete(page.completeness)
      setError(null)
    } catch (e) {
      setError(e)
    } finally {
      setLoading(false)
    }
  }
  useEffect(() => { void load(null, true) }, [id])

  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement).tagName === 'INPUT') return
      if (e.key === 'Escape' && selected !== null) onClose()
      if (e.key === 'j' || e.key === 'ArrowDown' && e.altKey) { const n = moveSelection(events, selected, 1); if (n !== null) onSelect(n) }
      if (e.key === 'k' || e.key === 'ArrowUp' && e.altKey) { const n = moveSelection(events, selected, -1); if (n !== null) onSelect(n) }
    }
    window.addEventListener('keydown', on)
    return () => window.removeEventListener('keydown', on)
  }, [events, selected])

  if (error && events.length === 0) return <Card><ErrorState error={error} onRetry={() => load(null, true)} /></Card>
  const capped = events.length >= MAX_LOADED
  return (
    <>
      {complete && !complete.complete && (
        <Callout tone="muted">The seq range this list covers overlaps something TailFlow recorded: {complete.reasons.map(r => (COMPLETENESS_REASON[r] ?? r).toLowerCase()).join('; ')}. The Coverage tab says whether it affects this session.</Callout>
      )}
      <div class={selected !== null ? 'with-inspector' : ''} style={{ height: 'min(70vh, 760px)', display: selected !== null ? undefined : 'flex' }}>
        <div class="card" style={{ display: 'flex', flexDirection: 'column', minHeight: 0, flex: 1 }}>
          {events.length === 0 && !loading
            ? <Empty title="No events retained for this session" />
            : <EventList events={events} selected={selected} onSelect={onSelect} label="Session events"
                footer={(hasMore && !capped) || capped ? (
                  <div style={{ padding: '12px', textAlign: 'center' }}>
                    {capped
                      ? <span class="note">Showing the first {MAX_LOADED.toLocaleString('en-US')} events. Use <a href={href('activity', { session: id })}>Activity</a> to filter this session.</span>
                      : <button class="btn" disabled={loading} onClick={() => load(cursor, false)}>{loading ? 'Loading…' : 'Load more'}</button>}
                  </div>
                ) : null} />}
        </div>
        {selected !== null && <Inspector seq={selected} onClose={onClose} onSelect={onSelect} />}
      </div>
    </>
  )
}

function Summary({ d }: { d: SessionDetail }) {
  const sampled = d.coverage.reasons.includes('summary_sampled_newest_events')
  const outcomes = useAsync(s => api.outcomes({ session: d.session_id, since: '30d', limit: 100 }, s), [d.session_id], 30_000,
    e => e.session_id === d.session_id && e.event_type.startsWith('command.'))
  const checks = (outcomes.data?.items ?? []).filter(i => ['test', 'build', 'lint', 'typecheck', 'format'].includes(i.category))
  return (
    <>
      <Card title="Tests, builds & checks" sub="Classified from the command; outcome only as the agent reported it" flush>
        {outcomes.error ? <ErrorState error={outcomes.error} onRetry={outcomes.reload} />
          : !outcomes.data ? <Loading rows={2} />
          : checks.length === 0 ? <Empty icon="commands" title="No test, build or check commands reported" />
          : <OutcomeTable items={checks} showSession={false} />}
      </Card>
      {sampled && <Callout tone="info">This session is very large: these summaries cover its newest 50,000 events.</Callout>}
      <div class="grid two">
        <Card title="Tools" sub="Finished calls by tool" flush>
          {d.tools.length === 0 ? <Empty title="No tool calls recorded" /> : (
            <table class="table"><tbody>{d.tools.map(t => <tr key={t.name}><td class="mono">{t.name}</td><td class="num">{count(t.count)}</td></tr>)}</tbody></table>
          )}
        </Card>
        <Card title="Programs" sub="Shell commands by program" flush>
          {d.programs.length === 0 ? <Empty title="No commands recorded" /> : (
            <table class="table"><tbody>{d.programs.map(t => (
              <tr key={t.name}><td class="mono"><a href={href('commands', { session: d.session_id, program: t.name })}>{t.name}</a></td><td class="num">{count(t.count)}</td></tr>
            ))}</tbody></table>
          )}
        </Card>
        <Card title="Reported files" sub="Most recent first · agent-reported, not verified on disk" flush>
          {d.files.length === 0 ? <Empty title="No file operations reported">
            {d.coverage.integration_limits.some(l => l.activity.startsWith('file.')) ? 'This integration cannot see every file operation — see Coverage.' : null}
          </Empty> : (
            <div class="table-wrap" style={{ maxHeight: '420px' }}>
              <table class="table"><tbody>{d.files.map(f => (
                <tr key={f.path}>
                  <td class="mono" title={f.path}>{relativeTo(f.path, d.git.root ?? d.project_id)}</td>
                  <td><Badge tone="info" icon={false}>{`Reported ${f.last_event_type.replace('file.', '')}`}</Badge></td>
                  <td class="num">{count(f.events)}</td>
                </tr>
              ))}</tbody></table>
            </div>
          )}
        </Card>
        <Card title="Failures" sub="Most recent reported failures" flush>
          {d.failures.length === 0 ? <Empty icon="check" title="No failures reported" /> : (
            <table class="table"><tbody>{d.failures.map(f => (
              <tr key={f.seq} class="clickable" onClick={() => navigate(href('session', { event: f.seq }, d.session_id))}>
                <td class="dim">{f.timestamp ? new Date(f.timestamp).toLocaleTimeString() : '—'}</td>
                <td class="wrap">{f.message ?? f.event_type}</td>
                <td class="num">{f.exit_code !== null ? `exit ${f.exit_code}` : ''}</td>
              </tr>
            ))}</tbody></table>
          )}
        </Card>
      </div>
    </>
  )
}

function Coverage({ d }: { d: SessionDetail }) {
  const c = d.coverage
  return (
    <div class="grid two">
      <Card title="Captured history" sub="What TailFlow recorded for this session">
        <dl class="kv">
          <dt>History</dt><dd><HistoryBadge history={c.history} /> <span class="note">{HISTORY_TEXT[c.history].detail}</span></dd>
          <dt>Start observed</dt><dd>{c.start_observed ? 'yes' : 'no'}</dd>
          <dt>End observed</dt><dd>{c.end_observed ? 'yes' : 'no'}</dd>
          <dt>Retention</dt><dd>{c.retention_truncated ? 'older events removed by retention' : 'all events retained'}</dd>
        </dl>
        {c.reasons.length > 0 && (
          <ul class="note" style={{ marginTop: '12px', paddingLeft: '18px' }}>
            {c.reasons.map(r => <li key={r}>{COVERAGE_REASON[r] ?? r}</li>)}
          </ul>
        )}
        <h4 style={{ margin: '16px 0 6px', fontSize: '12px' }}>Recorded event loss</h4>
        {c.gaps.length === 0 ? <p class="note">No loss recorded overlapping this session.</p> : (
          <ul style={{ listStyle: 'none', display: 'grid', gap: '6px' }}>
            {c.gaps.map((g, i) => (
              <li key={i}><Callout tone="error">{GAP_REASON[g.reason] ?? g.reason}{g.count !== null ? ` — ${count(g.count)} event(s)` : ' — unknown number'}{g.first_at ? `, ${new Date(g.first_at).toLocaleString()}` : ''}</Callout></li>
            ))}
          </ul>
        )}
      </Card>
      <Card title="Integration limits" sub={`What ${d.agent_name === 'codex' ? 'Codex' : 'Claude Code'} hooks cannot report — separate from loss`}>
        {c.integration_limits.length === 0 ? <p class="note">No known limits.</p> : c.integration_limits.map(l => (
          <div class="cap-row" key={l.activity}>
            <strong>{ACTIVITY_NAME[l.activity] ?? l.activity}</strong>
            <span><Badge tone={l.level === 'none' ? 'muted' : 'warn'} icon={l.level === 'none' ? 'eyeOff' : 'dashed'}>{LEVEL_TEXT[l.level]}</Badge></span>
            <span class="note">{l.note}</span>
          </div>
        ))}
        <p class="note" style={{ marginTop: '12px' }}>
          A count of zero for an activity that is only partly observable (or not observable) means "not seen", not "did not happen".
        </p>
      </Card>
    </div>
  )
}

/** Git worktree state before and after the session, with the evidence for
 *  each changed path. Never credits pre-existing changes. */
function Changes({ id }: { id: string }) {
  const c = useAsync<SessionChanges>(s => api.sessionChanges(id, s), [id], 30_000,
    e => e.event_type === 'git.snapshot' || e.session_id === id)
  const { settings } = useSettings()
  if (c.error) return <Card><ErrorState error={c.error} onRetry={c.reload} /></Card>
  if (!c.data) return <Card><Loading rows={4} /></Card>
  const v = c.data
  if (!v.available) {
    return <Card><Empty icon="branch" title="No Git evidence for this session">{v.reason}</Empty></Card>
  }
  const changes = v.changes ?? []
  const snap = (label: string, r: NonNullable<SessionChanges['baseline']>) => (
    <div>
      <div class="dim">{label}</div>
      <div><span class="mono">{r.head?.slice(0, 10) ?? 'no HEAD'}</span>{r.branch ? <> on <Badge tone="outline" icon="branch">{r.branch}</Badge></> : null}</div>
      <div class="note">{dateTime(r.at, settings.tz)} · {r.trigger?.replace('_', ' ') ?? 'snapshot'} · {count(r.changes_total)} path{r.changes_total === 1 ? '' : 's'} differ from HEAD{r.complete ? '' : ' (list partial)'}</div>
    </div>
  )
  return (
    <>
      <Card title="Worktree before and after" sub={v.repository}>
        <div class="grid two">
          {v.baseline && snap('Baseline', v.baseline)}
          {v.after && snap('After', v.after)}
        </div>
      </Card>
      {(v.limitations ?? []).length > 0 && (
        <Callout tone="muted">
          <div>
            <strong>What this can and cannot show</strong>
            <ul class="plain">{(v.limitations ?? []).map(l => <li key={l}>{l}</li>)}</ul>
          </div>
        </Callout>
      )}
      <Card title="Changed during the session" sub={`${count(changes.length)} path${changes.length === 1 ? '' : 's'} · pre-existing changes are listed separately`} flush>
        {changes.length === 0
          ? <Empty icon="branch" title="No worktree changes between the snapshots" />
          : (
            <div class="table-wrap">
              <table class="table">
                <thead><tr><th>Path</th><th>Change</th><th>Evidence</th><th class="num">Observed</th><th class="num hide-sm">Lines</th></tr></thead>
                <tbody>
                  {changes.map(ch => {
                    const after = ch.after ?? ch.before
                    const lines = after && (after.lines_added !== undefined || after.lines_deleted !== undefined)
                      ? `+${after.lines_added ?? '?'} −${after.lines_deleted ?? '?'}` : '—'
                    return (
                      <tr key={ch.path}>
                        <td class="mono" title={ch.path}>{ch.path}</td>
                        <td title={CHANGE_KIND[ch.kind]}>{after?.status ?? ch.kind} <span class="dim hide-sm">· {CHANGE_KIND[ch.kind]}</span></td>
                        <td>
                          <Badge tone={CHANGE_EVIDENCE[ch.evidence].tone} title={CHANGE_EVIDENCE[ch.evidence].detail}>{CHANGE_EVIDENCE[ch.evidence].label}</Badge>
                          {ch.other_sessions.length > 0 && <span class="note"> also {ch.other_sessions.map(o => o.split(':').pop()?.slice(0, 8)).join(', ')}</span>}
                        </td>
                        <td class="num" title="File watcher observations of this path between the snapshots">{count(ch.observations)}</td>
                        <td class="num hide-sm mono">{lines}</td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
            </div>
          )}
      </Card>
      {(v.pre_existing ?? []).length > 0 && (
        <Card title="Pre-existing changes" sub="Already different from HEAD at the baseline and unchanged since: not credited to this session">
          <ul class="plain mono">{(v.pre_existing ?? []).map(p => <li key={p}>{p}</li>)}</ul>
        </Card>
      )}
      {(v.concurrent_sessions ?? []).length > 0 && (
        <Card title="Other sessions in this repository" sub="Active between the snapshots">
          <span class="chips">{(v.concurrent_sessions ?? []).map(o => (
            <a key={o.session_id} href={href('session', {}, o.session_id)}><Agent name={o.agent} /> <span class="mono">{o.session_id.split(':').pop()?.slice(0, 8)}</span></a>
          ))}</span>
        </Card>
      )}
    </>
  )
}
