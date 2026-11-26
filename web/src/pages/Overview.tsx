import { useState } from 'preact/hooks'
import { isFailureEvent as isFailure, isSessionChange } from '../lib/liveFilters.ts'
import { api, ApiError } from '../lib/api.ts'
import { useAsync, useNow } from '../lib/hooks.ts'
import { href } from '../lib/router.ts'
import { compact, count, projectName, relative, shortId } from '../lib/format.ts'
import { agentLabel, LEVEL_TEXT, ACTIVITY_NAME, summary, statusOf } from '../lib/events.ts'
import type { AgentsStatus, Stats, StatsBucket } from '../lib/types.ts'
import {
  Agent, Badge, Callout, Card, Empty, ErrorState, HistoryBadge, Loading, Num, RANGES, Segmented,
  SessionStatusBadge, Stat, StatusBadge, Time,
} from '../components/ui'
import { Icon } from '../components/icons'

const POLL_MS = 15_000

export const INSTALL_TEXT = {
  configured: { label: 'Hooks configured', tone: 'ok' as const, detail: 'Every recorded hook is in the agent config and its binary exists.' },
  incomplete: { label: 'Needs attention', tone: 'warn' as const, detail: 'Hooks are missing, disabled, blocked by policy, or point at a missing binary.' },
  not_installed: { label: 'Not installed', tone: 'muted' as const, detail: 'No install record. Hooks added by hand are not detected here.' },
  unknown: { label: 'Unknown', tone: 'muted' as const, detail: 'The install record could not be read.' },
}

export function Overview() {
  const [range, setRange] = useState<string>('24h')
  // Live: any committed agent activity can change these aggregates.
  const any = () => true
  const stats = useAsync(s => api.stats({ since: range, buckets: 48 }, s), [range], POLL_MS, any)
  // Installation state is not agent activity: keep polling it.
  const agents = useAsync(s => api.agents(s), [], POLL_MS * 2)
  const sessions = useAsync(s => api.sessions({ limit: 8 }, s), [], POLL_MS, isSessionChange)
  const failures = useAsync(s => api.events({
    type: 'agent.tool.failed,command.failed,agent.turn.failed,runtime.error,system.events.dropped',
    since: range, order: 'desc', limit: 8,
  }, s), [range], POLL_MS, isFailure)
  useNow(30_000)

  const storageDown = stats.error instanceof ApiError && (stats.error.kind === 'storage_disabled' || stats.error.kind === 'storage_unavailable')
  const st = stats.data

  return (
    <div class="page">
      <div class="page-head">
        <div>
          <h2>Overview</h2>
          <p>What your coding agents did, and what your running stack logged, in the last {range}.</p>
        </div>
        <Segmented label="Time range" value={range} options={RANGES} onChange={setRange} />
      </div>

      {storageDown ? <Card><ErrorState error={stats.error} /></Card> : (
        <>
          {st && (st.retention_truncated || st.gaps_in_range > 0 || st.sessions.capped) && (
            <Callout tone="warn">
              {st.retention_truncated && <>This range reaches back before retained history (oldest retained: <Time at={st.history_starts_at} mode="full" />). Older activity existed and is not counted. </>}
              {st.gaps_in_range > 0 && <>{count(st.gaps_in_range)} recorded loss {st.gaps_in_range === 1 ? 'gap overlaps' : 'gaps overlap'} this range — counts are lower bounds. <a href={href('integrations')}>Details</a>. </>}
              {st.sessions.capped && <>More than 10,000 sessions overlap this range; session counts cover the most recent 10,000.</>}
            </Callout>
          )}
          <StatGrid st={st} agents={agents.data} range={range} error={stats.error} />
          <div class="grid main-side">
            <Card title="Activity over time" sub={st ? `${compact(st.events_ingested)} events ingested · ${st.bucket_secs >= 3600 ? `${Math.round(st.bucket_secs / 3600)} h` : `${Math.round(st.bucket_secs / 60)} min`} buckets` : ' '}>
              {st ? <Histogram buckets={st.buckets} /> : stats.error ? <ErrorState error={stats.error} onRetry={stats.reload} /> : <Loading rows={3} />}
            </Card>
            <Card title="Recent failures" sub="Reported failures and runtime errors" flush
              actions={<a class="btn sm ghost" href={href('errors', { since: range })}>View all</a>}>
              {failures.error ? <ErrorState error={failures.error} onRetry={failures.reload} />
                : !failures.data ? <Loading />
                : failures.data.events.length === 0 ? <Empty icon="check" title="No failures in this range">Nothing reported a failure and no runtime error was logged.</Empty>
                : (
                  <ul style={{ listStyle: 'none' }}>
                    {[...failures.data.events].sort((x, y) => Date.parse(y.timestamp) - Date.parse(x.timestamp)).map(e => (
                      <li key={e.seq}>
                        <a class="act-row" style={{ gridTemplateColumns: '60px minmax(0,1fr) auto', color: 'inherit', textDecoration: 'none' }}
                          href={href('errors', { since: range, event: e.seq })}>
                          <span class="t"><Time at={e.timestamp} /></span>
                          <span class="what" title={e.message ?? summary(e)}>
                            <span class="dim">{agentLabel(e.agent_name) === 'Runtime' ? e.source_id : agentLabel(e.agent_name)} · </span>{summary(e)}
                          </span>
                          <StatusBadge info={statusOf(e)} />
                        </a>
                      </li>
                    ))}
                  </ul>
                )}
            </Card>
          </div>
          <div class="grid two">
            {(st?.agents ?? []).filter(a => a.agent === 'claude-code' || a.agent === 'codex').map(a => (
              <AgentCard key={a.agent} a={a} status={agents.data} range={range} />
            ))}
            {!st && !stats.error && <><Card><Loading /></Card><Card><Loading /></Card></>}
          </div>
          <Card title="Recent sessions" flush actions={<a class="btn sm ghost" href={href('sessions')}>All sessions</a>}>
            {sessions.error ? <ErrorState error={sessions.error} onRetry={sessions.reload} />
              : !sessions.data ? <Loading />
              : sessions.data.sessions.length === 0 ? (
                <Empty icon="sessions" title="No agent sessions yet">
                  Install a hook with <code>tailflow agent install claude</code> or <code>tailflow agent install codex</code>, then start the agent. Sessions appear here as soon as the daemon ingests their first event.
                </Empty>
              ) : (
                <div class="table-wrap">
                  <table class="table">
                    <thead><tr>
                      <th>Agent</th><th>Project</th><th class="hide-sm">Started</th><th>Last activity</th><th>Status</th>
                      <th class="num">Tools</th><th class="num">Failures</th><th class="hide-sm">History</th>
                    </tr></thead>
                    <tbody>
                      {sessions.data.sessions.map(s => (
                        <tr key={s.session_id} class="clickable" tabIndex={0}
                          onClick={() => { location.hash = href('session', {}, s.session_id) }}
                          onKeyDown={e => { if (e.key === 'Enter') location.hash = href('session', {}, s.session_id) }}>
                          <td><Agent name={s.agent_name} /></td>
                          <td title={s.project_id ?? ''}><div class="cell-main">{projectName(s.project_id)}<span class="sub">{shortId(s.session_id)}{s.git.branch ? ` · ${s.git.branch}` : ''}</span></div></td>
                          <td class="hide-sm dim"><Time at={s.started_at ?? s.first_event_at} /></td>
                          <td class="dim">{relative(s.last_event_at)}</td>
                          <td><SessionStatusBadge status={s.status} /></td>
                          <Num n={s.counts.tool_calls} />
                          <Num n={s.counts.failed_operations} bad />
                          <td class="hide-sm"><HistoryBadge history={s.coverage.history} /></td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
          </Card>
        </>
      )}
    </div>
  )
}

function StatGrid({ st, agents, range, error }: { st: Stats | null; agents: AgentsStatus | null; range: string; error: unknown }) {
  if (!st) {
    return (
      <div class="grid stats">
        {Array.from({ length: 8 }, (_, i) => <div class="card stat" key={i}><div class="skeleton" style={{ width: '60%' }} /><div class="skeleton" style={{ height: '26px', width: '40%', marginTop: '8px' }} /></div>)}
        {error ? <span class="sr-only">{String((error as Error).message ?? error)}</span> : null}
      </div>
    )
  }
  const a = st.activity
  const installed = agents?.agents.filter(x => x.installation?.state === 'configured').length ?? null
  const withActivity = st.agents.filter(x => x.sessions > 0).length
  return (
    <div class="grid stats eight">
      <Stat label="Active sessions" icon="play2" value={count(st.sessions.active)} href={href('sessions', { status: 'active' })}
        foot="activity in the last 30 min, no end seen" tone={st.sessions.active > 0 ? 'ok' : undefined} />
      <Stat label="Sessions" icon="sessions" value={count(st.sessions.total)} href={href('sessions', { since: range })}
        foot={`${count(st.sessions.ended)} ended · ${count(st.sessions.unterminated)} no end seen`} />
      <Stat label="Tool calls" icon="tool" value={compact(a.tool_calls)} href={href('activity', { type: 'tools', since: range })}
        foot={a.tool_failures ? `${count(a.tool_failures)} failed` : 'finished calls'} />
      <Stat label="Commands" icon="commands" value={compact(a.commands)} href={href('commands', { since: range })}
        foot={a.command_failures ? `${count(a.command_failures)} reported failing` : 'shell commands started'} />
      <Stat label="Reported file changes" icon="files" value={compact(a.file_changes)} href={href('files', { since: range })}
        foot={`${compact(a.file_reads)} reads reported · not verified`} />
      <Stat label="Failed operations" icon="errors" value={count(a.failures)} tone={a.failures > 0 ? 'error' : undefined}
        href={href('errors', { since: range })} foot={`${count(st.runtime_errors)} runtime errors logged`} />
      <Stat label="Events ingested" icon="database" value={compact(st.events_ingested)} href={href('logs')}
        foot={`${compact(st.runtime_warnings)} warnings · all sources`} />
      <Stat label="Integrations" icon="integrations" value={installed === null ? '—' : `${installed} / ${agents?.agents.length ?? 2}`}
        href={href('integrations')} foot={`hooks configured · ${withActivity} with sessions`} />
    </div>
  )
}

function Histogram({ buckets }: { buckets: StatsBucket[] }) {
  if (buckets.length === 0) return <Empty title="No data" />
  const max = Math.max(1, ...buckets.map(b => b.ingested + b.agent_events))
  const w = 100 / buckets.length
  const total = buckets.reduce((n, b) => n + b.ingested, 0)
  if (total === 0) return <Empty icon="clock" title="Nothing ingested in this range">Start the daemon with a source (or an agent session) and activity will show up here.</Empty>
  return (
    <>
      <svg class="spark" viewBox="0 0 100 72" preserveAspectRatio="none" role="img"
        aria-label={`Events per bucket; peak ${max.toLocaleString('en-US')}`}>
        {buckets.map((b, i) => {
          const h = ((b.ingested) / max) * 68
          const eh = ((b.failures + b.runtime_errors) / max) * 68
          return (
            <g key={i}>
              <title>{`${b.start ? new Date(b.start).toLocaleString() : ''}: ${b.ingested} ingested, ${b.agent_events} agent events, ${b.failures} failures, ${b.runtime_errors} runtime errors`}</title>
              <rect class="bar" x={i * w + w * 0.1} width={w * 0.8} y={72 - h} height={h} />
              {eh > 0 && <rect class="bar err" x={i * w + w * 0.1} width={w * 0.8} y={72 - Math.max(eh, 1.5)} height={Math.max(eh, 1.5)} />}
            </g>
          )
        })}
      </svg>
      <div class="axis-labels">
        <Time at={buckets[0].start} />
        <span>now</span>
      </div>
      <div class="legend" style={{ marginTop: '8px' }}>
        <span><i style={{ background: 'var(--accent)', opacity: 0.55 }} />Events ingested</span>
        <span><i style={{ background: 'var(--error)' }} />Failures + runtime errors</span>
      </div>
    </>
  )
}

function AgentCard({ a, status, range }: { a: Stats['agents'][number]; status: AgentsStatus | null; range: string }) {
  const info = status?.agents.find(x => x.agent === a.agent)
  const inst = info?.installation ? INSTALL_TEXT[info.installation.state] : null
  const limits = (info?.capabilities ?? []).filter(c => c.level !== 'direct')
  return (
    <Card title={<Agent name={a.agent} />} sub={a.latest_activity_at ? `last activity ${relative(a.latest_activity_at)}` : 'no sessions recorded'}
      actions={inst && <Badge tone={inst.tone} title={inst.detail}>{inst.label}</Badge>}>
      <dl class="kv">
        <dt>Latest session</dt>
        <dd>{a.latest_session_id ? (
          <span class="toolbar">
            <a href={href('session', {}, a.latest_session_id)} class="mono">{shortId(a.latest_session_id, 12)}</a>
            {a.latest_session_status && <SessionStatusBadge status={a.latest_session_status} />}
          </span>
        ) : <span class="dim">none</span>}</dd>
        <dt>Sessions ({range})</dt><dd>{count(a.sessions)}{a.active_sessions > 0 && <> · <span style={{ color: 'var(--ok)' }}>{a.active_sessions} active</span></>}</dd>
        <dt>Tool calls</dt><dd>{count(a.tool_calls)}{a.tool_failures > 0 && <span style={{ color: 'var(--error)' }}> · {count(a.tool_failures)} failed</span>}</dd>
        <dt>Commands</dt><dd>{count(a.commands)}{a.command_failures > 0 && <span style={{ color: 'var(--error)' }}> · {count(a.command_failures)} reported failing</span>}</dd>
        <dt>Reported files</dt><dd>{count(a.file_changes)} changes · {count(a.file_reads)} reads</dd>
      </dl>
      {limits.length > 0 && (
        <div style={{ marginTop: '12px' }}>
          <div class="note" style={{ marginBottom: '6px' }}>Coverage limits — zero here can mean "not observable":</div>
          <div class="chips">
            {limits.map(c => (
              <Badge key={c.activity} tone={c.level === 'none' ? 'muted' : 'warn'} icon={c.level === 'none' ? 'eyeOff' : 'dashed'}
                title={c.note}>{`${ACTIVITY_NAME[c.activity] ?? c.activity}: ${LEVEL_TEXT[c.level].toLowerCase()}`}</Badge>
            ))}
          </div>
        </div>
      )}
      {!inst && !status && <p class="note"><Icon name="info" size="sm" /> Integration status loading…</p>}
    </Card>
  )
}
