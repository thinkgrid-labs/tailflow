import { useMemo, useState } from 'preact/hooks'
import { api, describeError } from '../lib/api.ts'
import { useAsync, navigate } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { count, duration, between, shortPath, shortId } from '../lib/format.ts'
import { attrString, exitCode } from '../lib/events.ts'
import type { StoredEvent, EventPage } from '../lib/types.ts'
import { Agent, Badge, Callout, Card, CopyButton, Empty, ErrorState, Loading, RANGES, Segmented, Time } from '../components/ui'
import { Inspector } from '../components/Inspector'
import { Icon } from '../components/icons'

/** With payloads (the command line, when the capture mode keeps it) pages are ≤ 100. */
const PAGE = 100

export interface CommandRow {
  key: string
  start: StoredEvent | null
  end: StoredEvent | null
}

/** Pair `command.started` with its end event by operation id. */
export function pairCommands(events: readonly StoredEvent[]): CommandRow[] {
  const rows = new Map<string, CommandRow>()
  for (const e of events) {
    const k = e.operation_id ? `${e.session_id}|${e.operation_id}` : `seq:${e.seq}`
    const row = rows.get(k) ?? { key: k, start: null, end: null }
    if (e.event_type === 'command.started') row.start = e
    else row.end = e
    rows.set(k, row)
  }
  return [...rows.values()].sort((a, b) =>
    Date.parse((b.start ?? b.end)!.timestamp) - Date.parse((a.start ?? a.end)!.timestamp))
}

function outcome(r: CommandRow): { label: string; tone: 'ok' | 'error' | 'warn' | 'muted'; detail: string } {
  const e = r.end
  if (!e) {
    const codex = r.start?.agent_name === 'codex'
    return codex
      ? { label: 'Outcome not reported', tone: 'muted', detail: 'Codex reports a command result only when the response carries an exit code.' }
      : { label: 'No end event yet', tone: 'muted', detail: 'Still running, interrupted without a hook call, or the end was lost.' }
  }
  if (e.event_type === 'command.completed') {
    return exitCode(e) === 0
      ? { label: 'Exit 0', tone: 'ok', detail: 'Exit code 0 as reported by the agent.' }
      : { label: 'Reported success', tone: 'ok', detail: 'The agent reported success without an exit code (Claude Code: the success hook fired, not the failure hook).' }
  }
  if (e.status === 'cancelled') return { label: 'Interrupted', tone: 'warn', detail: 'The agent reported an interruption.' }
  const code = exitCode(e)
  return { label: code !== undefined ? `Exit ${code}` : 'Failed', tone: 'error', detail: code !== undefined ? 'Exit code as reported (parsed from the agent\'s error text for Claude Code).' : 'Reported failure without an exit code.' }
}

export function Commands({ params }: { params: URLSearchParams }) {
  const p = toRecord(params)
  const since = p.since ?? '24h'
  const selected = p.event ? Number(p.event) : null
  const set = (c: Record<string, string | number | null>) => navigate(href('commands', withParams(params, c)), true)
  const q = { type: 'command.*', since: since === 'all' ? undefined : since, agent: p.agent, session: p.session, program: p.program, include_payload: true }
  const key = JSON.stringify(q)
  const [extra, setExtra] = useState<{ key: string; events: StoredEvent[]; page: EventPage | null }>({ key, events: [], page: null })
  // Poll only while the list is a single page: see Activity.
  const olderLoaded = extra.key === key && extra.events.length > 0
  const first = useAsync(s => api.events({ ...q, order: 'desc', limit: PAGE }, s), [key], olderLoaded ? undefined : 20_000)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<unknown>(null)
  const ext = extra.key === key ? extra : { key, events: [], page: null }
  const last = ext.page ?? first.data
  const rows = useMemo(() => {
    const seen = new Set<number>()
    const all = [...(first.data?.events ?? []), ...ext.events].filter(e => (seen.has(e.seq) ? false : (seen.add(e.seq), true)))
    return pairCommands(all)
  }, [first.data, ext.events])
  const metadataOnly = rows.length > 0 && rows.every(r => !(r.start?.payload ?? r.end?.payload))

  const more = async () => {
    if (!last?.has_more || last.next_cursor === null) return
    setBusy(true)
    try {
      const pg = await api.events({ ...q, order: 'desc', limit: PAGE, before: last.next_cursor })
      setExtra({ key, events: [...ext.events, ...pg.events], page: pg })
      setErr(null)
    } catch (e) { setErr(e) } finally { setBusy(false) }
  }

  return (
    <div class="page" style={{ maxWidth: 'none' }}>
      <div class="page-head">
        <div>
          <h2>Commands</h2>
          <p>Shell commands agents reported running. Status, exit code and duration appear only when the agent reported them.</p>
        </div>
        <CopyButton text={location.href} label="Copy filter URL" />
      </div>
      <form class="toolbar" onSubmit={e => {
        e.preventDefault()
        const v = (new FormData(e.currentTarget as HTMLFormElement).get('program') as string).trim()
        set({ program: v || null, event: null })
      }}>
        <input class="input" name="program" type="search" defaultValue={p.program ?? ''} placeholder="Program, e.g. cargo" aria-label="Program" />
        <select class="select" aria-label="Agent" value={p.agent ?? ''} onChange={e => set({ agent: (e.target as HTMLSelectElement).value || null })}>
          <option value="">All agents</option><option value="claude-code">Claude Code</option><option value="codex">Codex</option>
        </select>
        <Segmented label="Time range" value={since} options={[...RANGES, { value: 'all', label: 'All' }]} onChange={v => set({ since: v === '24h' ? null : v })} />
        {p.session && <span class="chip"><span>session: {shortId(p.session, 12)}</span><button aria-label="Remove session filter" onClick={() => set({ session: null })}><Icon name="x" size="sm" /></button></span>}
      </form>
      {metadataOnly && <Callout tone="muted">Command lines are not stored in <code>metadata</code> capture mode (the default). Program names, exit codes and durations still are. See Integrations → Privacy.</Callout>}
      <div class={selected !== null ? 'with-inspector' : ''}>
        <Card flush>
          {first.error ? <ErrorState error={first.error} onRetry={first.reload} />
            : !first.data ? <Loading rows={6} />
            : rows.length === 0 ? <Empty icon="commands" title="No commands reported in this range" />
            : (
              <div class="table-wrap">
                <table class="table">
                  <thead><tr>
                    <th>Started</th><th>Program</th><th class="hide-sm">Command</th><th>Agent</th><th>Status</th>
                    <th class="num">Duration</th><th class="hide-sm">Directory</th><th class="hide-sm">Session</th>
                  </tr></thead>
                  <tbody>
                    {rows.map(r => {
                      const e = r.start ?? r.end!
                      const o = outcome(r)
                      const dur = r.end?.duration_ms ?? (r.start && r.end ? between(r.start.timestamp, r.end.timestamp) : null)
                      const line = r.start?.payload ?? r.end?.payload
                      const seq = (r.end ?? r.start)!.seq
                      return (
                        <tr key={r.key} class="clickable" tabIndex={0} aria-selected={selected === seq}
                          onClick={() => set({ event: seq })} onKeyDown={ev => { if (ev.key === 'Enter') set({ event: seq }) }}>
                          <td class="dim">{r.start ? <Time at={r.start.timestamp} /> : <span title="Start not captured">— <Time at={e.timestamp} /></span>}</td>
                          <td class="mono">{attrString(e, 'command.program') ?? '?'}{e.attributes?.['command.compound'] === true && <Badge tone="outline" title="Several commands joined with && ; | — the program is the first one">+</Badge>}</td>
                          <td class="hide-sm mono dim" title={line ?? 'not stored'} style={{ maxWidth: '360px' }}>{line ?? '—'}</td>
                          <td><Agent name={e.agent_name} /></td>
                          <td><Badge tone={o.tone} title={o.detail}>{o.label}</Badge></td>
                          <td class="num">{dur !== null && dur !== undefined ? duration(dur) : <span class="dim" title="Not reported">—</span>}</td>
                          <td class="hide-sm dim mono" title={e.path ?? ''}>{shortPath(e.path)}</td>
                          <td class="hide-sm"><a href={href('session', {}, e.session_id ?? '')} onClick={ev => ev.stopPropagation()} class="mono">{shortId(e.session_id)}</a></td>
                        </tr>
                      )
                    })}
                  </tbody>
                </table>
              </div>
            )}
          {last?.has_more && (
            <div style={{ padding: '12px', textAlign: 'center' }}>
              <button class="btn" disabled={busy} onClick={more}>{busy ? 'Loading…' : 'Load older'}</button>
              {err ? <p class="note warn">{describeError(err)}</p> : null}
            </div>
          )}
        </Card>
        {selected !== null && <Inspector seq={selected} onClose={() => set({ event: null })} onSelect={s => set({ event: s })} />}
      </div>
      {olderLoaded && <button class="btn sm ghost" style={{ alignSelf: 'flex-start' }} onClick={() => { setExtra({ key, events: [], page: null }); first.reload() }}><Icon name="refresh" size="sm" />Back to newest</button>}
      {first.data && <p class="note">{count(rows.length)} command{rows.length === 1 ? '' : 's'} loaded. A command whose start and end fall on different pages appears once each until both are loaded.</p>}
    </div>
  )
}
