import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { matchesQuery } from '../lib/liveFilters.ts'
import { api, describeError } from '../lib/api.ts'
import { useAsync, navigate } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { count } from '../lib/format.ts'
import { COMPLETENESS_REASON } from '../lib/events.ts'
import type { EventPage, StoredEvent } from '../lib/types.ts'
import { Callout, CopyButton, Empty, ErrorState, Loading, RANGES, Segmented } from '../components/ui'
import { EventList, moveSelection } from '../components/EventList'
import { Inspector } from '../components/Inspector'
import { Icon } from '../components/icons'

const PAGE = 200
/** Rows kept in the browser for one search; beyond this, refine the filters. */
const MAX_LOADED = 5_000

/** Named categories → API `type` filter. */
export const TYPE_GROUPS: Record<string, { label: string; types: string }> = {
  all: { label: 'All', types: '' },
  agent: { label: 'Agent', types: 'agent.*,command.*,file.*' },
  tools: { label: 'Tools', types: 'agent.tool.*' },
  commands: { label: 'Commands', types: 'command.*' },
  files: { label: 'Files', types: 'file.*' },
  sessions: { label: 'Sessions', types: 'agent.session.*,agent.subagent.*' },
  runtime: { label: 'Runtime', types: 'runtime.*' },
  failures: {
    label: 'Failures',
    types: 'agent.tool.failed,command.failed,agent.turn.failed,runtime.error,system.events.dropped,system.collector.error',
  },
}

/** Filters a search box accepts as `key:value`. Everything else is full text. */
export const FILTER_KEYS: { key: string; hint: string }[] = [
  { key: 'agent', hint: 'claude-code, codex' },
  { key: 'session', hint: 'session id' },
  { key: 'project', hint: 'project path (exact)' },
  { key: 'type', hint: Object.keys(TYPE_GROUPS).join(', ') + ' or an event type' },
  { key: 'status', hint: 'started, succeeded, failed, cancelled' },
  { key: 'severity', hint: 'minimum: warn, error…' },
  { key: 'source', hint: 'runtime source label' },
  { key: 'path', hint: 'glob, ** crosses /' },
  { key: 'program', hint: 'command program, e.g. cargo' },
  { key: 'since', hint: '30m, 6h, 7d or RFC 3339' },
]
const KEYS = new Set(FILTER_KEYS.map(f => f.key))

/** Split `agent:codex type:files cargo test` into filters and free text. */
export function parseSearch(input: string): { filters: Record<string, string>; text: string } {
  const filters: Record<string, string> = {}
  const rest: string[] = []
  for (const tok of input.match(/(?:[^\s"]+|"[^"]*")+/g) ?? []) {
    const m = /^([a-z]+):(.+)$/.exec(tok)
    if (m && KEYS.has(m[1])) filters[m[1]] = m[2].replace(/^"|"$/g, '')
    else rest.push(tok)
  }
  return { filters, text: rest.join(' ') }
}

function apiParams(p: Record<string, string>, mode: 'activity' | 'errors') {
  const group = p.type ?? (mode === 'errors' ? 'failures' : 'all')
  const types = TYPE_GROUPS[group]?.types ?? group
  return {
    q: p.q, agent: p.agent, session: p.session, project: p.project, type: types || undefined,
    status: p.status, severity: p.severity, source: p.source, path: p.path, program: p.program,
    since: p.since === 'all' ? undefined : p.since ?? (p.session ? undefined : '24h'),
  }
}

export function Activity({ params, mode }: { params: URLSearchParams; mode: 'activity' | 'errors' }) {
  const page: 'activity' | 'errors' = mode
  const p = toRecord(params)
  const selected = p.event ? Number(p.event) : null
  const set = (c: Record<string, string | number | null>, replace = true) => navigate(href(page, withParams(params, c)), replace)

  const query = apiParams(p, mode)
  const key = JSON.stringify(query)
  const [extra, setExtra] = useState<{ key: string; events: StoredEvent[]; page: EventPage | null }>({ key, events: [], page: null })
  // Polling refreshes only the first page; once older pages are loaded it
  // stops, so the list stays one contiguous range (a refreshed first page
  // followed by pages from the old cursor would silently skip events).
  const olderLoaded = extra.key === key && extra.events.length > 0
  const first = useAsync(s => api.events({ ...query, order: 'desc', limit: PAGE }, s), [key],
    mode === 'errors' && !olderLoaded ? 20_000 : undefined,
    olderLoaded ? undefined : e => matchesQuery(e, query))
  const [moreErr, setMoreErr] = useState<unknown>(null)
  const [loadingMore, setLoadingMore] = useState(false)
  const ext = extra.key === key ? extra : { key, events: [], page: null }
  const last = ext.page ?? first.data
  const events = useMemo(() => {
    const seen = new Set<number>()
    return [...(first.data?.events ?? []), ...ext.events].filter(e => (seen.has(e.seq) ? false : (seen.add(e.seq), true)))
  }, [first.data, ext.events])
  const capped = events.length >= MAX_LOADED

  const loadMore = async () => {
    if (!last?.has_more || last.next_cursor === null || loadingMore || capped) return
    setLoadingMore(true)
    try {
      const pg = await api.events({ ...query, order: 'desc', limit: PAGE, before: last.next_cursor })
      setExtra({ key, events: [...ext.events, ...pg.events], page: pg })
      setMoreErr(null)
    } catch (e) { setMoreErr(e) } finally { setLoadingMore(false) }
  }

  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      if (/^(INPUT|SELECT|TEXTAREA)$/.test((e.target as HTMLElement).tagName)) return
      if (e.key === 'Escape' && selected !== null) set({ event: null })
      if (e.key === 'j') { const n = moveSelection(events, selected, 1); if (n !== null) set({ event: n }) }
      if (e.key === 'k') { const n = moveSelection(events, selected, -1); if (n !== null) set({ event: n }) }
    }
    window.addEventListener('keydown', on)
    return () => window.removeEventListener('keydown', on)
  }, [events, selected, params.toString()])

  const chips = Object.entries(p).filter(([k]) => KEYS.has(k) || k === 'q')
  const group = p.type ?? (mode === 'errors' ? 'failures' : 'all')
  const completeness = last?.completeness

  return (
    <div class="page full" style={{ display: 'flex', flexDirection: 'column' }}>
      <div class="page-head">
        <div>
          <h2>{mode === 'errors' ? 'Errors' : 'Activity'}</h2>
          <p>{mode === 'errors'
            ? 'Reported tool, command and turn failures, runtime errors, and TailFlow loss markers — newest first.'
            : 'Agent operations and runtime logs from durable history, filtered by the indexed query engine — newest first.'}</p>
        </div>
        <CopyButton text={location.href} label="Copy filter URL" />
      </div>
      <SearchBox value={[...Object.entries(p).filter(([k]) => KEYS.has(k)).map(([k, v]) => `${k}:${v.includes(' ') ? `"${v}"` : v}`), p.q ?? ''].filter(Boolean).join(' ')}
        onSubmit={(filters, text) => {
          const changes: Record<string, string | null> = { q: text || null, event: null }
          for (const k of KEYS) changes[k] = filters[k] ?? null
          set(changes, false)
        }} />
      <div class="toolbar">
        <Segmented label="Event category" value={TYPE_GROUPS[group] ? group : 'all'}
          options={Object.entries(TYPE_GROUPS).map(([value, g]) => ({ value, label: g.label }))}
          onChange={v => set({ type: v === (mode === 'errors' ? 'failures' : 'all') ? null : v, event: null })} />
        <Segmented label="Time range" value={p.since === 'all' ? '' : p.since ?? (p.session ? '' : '24h')}
          options={[...RANGES, { value: '', label: 'All' }]} onChange={v => set({ since: v === '24h' ? null : v || 'all', event: null })} />
      </div>
      {chips.length > 0 && (
        <div class="chips" aria-label="Active filters">
          {chips.map(([k, v]) => (
            <span class="chip" key={k}>
              <span>{k === 'q' ? `text: ${v}` : `${k}: ${v}`}</span>
              <button aria-label={`Remove ${k} filter`} onClick={() => set({ [k]: null, event: null })}><Icon name="x" size="sm" /></button>
            </span>
          ))}
          <button class="btn sm ghost" onClick={() => navigate(href(page), true)}>Clear all</button>
        </div>
      )}
      {completeness && !completeness.complete && (
        <Callout tone="warn">
          Results may be incomplete: {completeness.reasons.map(r => COMPLETENESS_REASON[r] ?? r).join('; ')}
          {completeness.gaps_total > 0 && ` (${count(completeness.gaps_total)} recorded gap${completeness.gaps_total === 1 ? '' : 's'})`}.
        </Callout>
      )}
      <div class="toolbar note" aria-live="polite">
        {first.data && <span>{count(events.length)} event{events.length === 1 ? '' : 's'} loaded{last?.has_more ? ' · more available' : ''}{last?.scan_budget_exhausted ? ' · scan budget reached, load more to continue' : ''}</span>}
        {first.loading && first.data && <span>Refreshing…</span>}
        {olderLoaded && <button class="btn sm ghost" onClick={() => { setExtra({ key, events: [], page: null }); first.reload() }}><Icon name="refresh" size="sm" />Newest</button>}
      </div>
      <div class={selected !== null ? 'with-inspector' : ''} style={{ flex: 1, minHeight: '320px', display: selected !== null ? undefined : 'flex' }}>
        <div class="card" style={{ display: 'flex', flexDirection: 'column', minHeight: 0, flex: 1, position: 'relative' }}>
          {first.loading && <div class="loading-bar" aria-hidden="true" />}
          {first.error ? <ErrorState error={first.error} onRetry={first.reload} />
            : !first.data ? <Loading rows={8} />
            : events.length === 0 ? (
              <Empty icon={mode === 'errors' ? 'check' : 'search'} title={mode === 'errors' ? 'No failures in this range' : 'No events match'}>
                {mode === 'errors' ? 'Nothing reported a failure and no runtime error was logged. Codex does not report failed tool calls, so their absence is not proof of success.' : 'Widen the time range or remove a filter.'}
              </Empty>
            ) : (
              <EventList events={events} selected={selected} label={mode === 'errors' ? 'Errors' : 'Activity'}
                onSelect={seq => set({ event: seq })}
                footer={
                  <div style={{ padding: '12px', textAlign: 'center' }}>
                    {capped ? <span class="note">{count(MAX_LOADED)} rows loaded — refine the filters to see older events.</span>
                      : last?.has_more ? <button class="btn" disabled={loadingMore} onClick={loadMore}>{loadingMore ? 'Loading…' : 'Load older'}</button>
                      : <span class="note">End of matching history{completeness?.history_starts_at ? ` (retained since ${new Date(completeness.history_starts_at).toLocaleString()})` : ''}.</span>}
                    {moreErr ? <p class="note warn" role="alert">{describeError(moreErr)}</p> : null}
                  </div>
                } />
            )}
        </div>
        {selected !== null && <Inspector seq={selected} onClose={() => set({ event: null })} onSelect={seq => set({ event: seq })} />}
      </div>
    </div>
  )
}

function SearchBox({ value, onSubmit }: { value: string; onSubmit: (filters: Record<string, string>, text: string) => void }) {
  const [text, setText] = useState(value)
  const [open, setOpen] = useState(false)
  const [active, setActive] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => setText(value), [value])

  const word = text.split(/\s+/).pop() ?? ''
  const suggestions = word.includes(':') ? [] : FILTER_KEYS.filter(f => f.key.startsWith(word.toLowerCase()))
  const accept = (k: string) => {
    const parts = text.split(/\s+/)
    parts[parts.length - 1] = `${k}:`
    setText(parts.join(' '))
    setActive(0)
    input.current?.focus()
  }
  return (
    <form class="toolbar" role="search" style={{ position: 'relative' }} onSubmit={e => {
      e.preventDefault()
      const { filters, text: t } = parseSearch(text)
      setOpen(false)
      onSubmit(filters, t)
    }}>
      <div class="grow" style={{ position: 'relative' }}>
        <input ref={input} class="input" style={{ width: '100%' }} type="search" value={text} spellcheck={false}
          placeholder="Full-text search, or filters like  agent:codex  type:failures  path:src/**"
          role="combobox" aria-controls="search-suggest" aria-label="Search history" aria-autocomplete="list" aria-expanded={open && suggestions.length > 0}
          onFocus={() => setOpen(true)} onBlur={() => setTimeout(() => setOpen(false), 150)}
          onInput={e => { setText((e.target as HTMLInputElement).value); setOpen(true) }}
          onKeyDown={e => {
            if (!open || suggestions.length === 0) return
            if (e.key === 'ArrowDown') { e.preventDefault(); setActive(a => Math.min(suggestions.length - 1, a + 1)) }
            if (e.key === 'ArrowUp') { e.preventDefault(); setActive(a => Math.max(0, a - 1)) }
            if (e.key === 'Tab' && word) { e.preventDefault(); accept(suggestions[active]?.key ?? suggestions[0].key) }
            if (e.key === 'Escape') setOpen(false)
          }} />
        {open && suggestions.length > 0 && (
          <div class="suggest" id="search-suggest" role="listbox" aria-label="Filter suggestions">
            {suggestions.map((s, i) => (
              <button type="button" role="option" aria-selected={i === active} key={s.key} onMouseDown={e => { e.preventDefault(); accept(s.key) }}>
                <span class="mono">{s.key}:</span><span class="hint">{s.hint}</span>
              </button>
            ))}
          </div>
        )}
      </div>
      <button class="btn primary" type="submit"><Icon name="search" size="sm" />Search</button>
    </form>
  )
}
