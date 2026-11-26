import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLogStream } from '../hooks/useLogStream'
import { compileFilter, MAX_RECORDS } from '../lib/logStore.ts'
import { useSettings, copyText } from '../lib/hooks.ts'
import { clock, count } from '../lib/format.ts'
import { href } from '../lib/router.ts'
import type { LogLevel, LogRecord } from '../lib/types.ts'
import { VirtualList, type VirtualHandle } from '../components/VirtualList'
import { Callout, Empty } from '../components/ui'
import { Icon } from '../components/icons'

const LEVELS: LogLevel[] = ['error', 'warn', 'info', 'debug', 'trace']
const PALETTE = ['#22d3ee', '#4ade80', '#facc15', '#e879f9', '#60a5fa', '#fb923c', '#a3e635', '#f472b6', '#2dd4bf', '#c084fc']

const colorIndex = new Map<string, number>()
function sourceColor(name: string): string {
  if (!colorIndex.has(name)) colorIndex.set(name, colorIndex.size % PALETTE.length)
  return PALETTE[colorIndex.get(name) ?? 0]
}

function parseJson(s: string): unknown {
  const t = s.trim()
  if (t[0] !== '{' && t[0] !== '[') return null
  try { return JSON.parse(t) } catch { return null }
}

export function Logs() {
  const live = useLogStream()
  const { settings, update } = useSettings()
  const [filter, setFilter] = useState('')
  const [levels, setLevels] = useState<Set<LogLevel> | null>(null)
  const [sources, setSources] = useState<Set<string> | null>(null)
  const [follow, setFollow] = useState(true)
  const [paused, setPaused] = useState(false)
  const [expanded, setExpanded] = useState<Set<number>>(new Set())
  const [copied, setCopied] = useState<string | null>(null)
  const list = useRef<VirtualHandle | null>(null)

  useEffect(() => { live.setFrozen(paused || !follow) }, [paused, follow])
  useEffect(() => () => live.setFrozen(false), [])

  const records = live.records
  const sourceCounts = useMemo(() => {
    const m = new Map<string, number>()
    for (const r of records) m.set(r.source, (m.get(r.source) ?? 0) + 1)
    return m
  }, [records])
  const match = useMemo(() => compileFilter(filter), [filter])
  const shown = useMemo(() => records.filter(r =>
    (!sources || sources.has(r.source)) &&
    (!levels || levels.has(r.level === 'unknown' ? 'trace' : r.level)) &&
    (!match || match(r))), [records, sources, levels, match])

  // Drop expansion state for rows no longer held (bounded memory).
  useEffect(() => {
    if (expanded.size === 0 || records.length === 0) return
    const first = records[0].seq
    if ([...expanded].some(s => s < first)) setExpanded(new Set([...expanded].filter(s => s >= first)))
  }, [records])

  const toggleLevel = (l: LogLevel) => setLevels(prev => {
    const next = new Set(prev ?? LEVELS)
    if (next.has(l)) next.delete(l); else next.add(l)
    return next.size === LEVELS.length ? null : next
  })
  const toggleSource = (s: string) => setSources(prev => {
    if (prev === null) return new Set([s])
    const next = new Set(prev)
    if (next.has(s)) next.delete(s); else next.add(s)
    return next.size === 0 ? null : next
  })

  const copyShown = async () => {
    const text = shown.map(r => `${r.timestamp} ${r.source} ${r.level.toUpperCase()} ${r.payload}`).join('\n')
    const ok = await copyText(text)
    setCopied(ok ? `Copied ${count(shown.length)} lines` : 'Copy failed')
    setTimeout(() => setCopied(null), 1800)
  }

  const resume = () => { setPaused(false); setFollow(true); list.current?.scrollToBottom() }
  const historyHref = !filter || filter.startsWith('/') ? href('activity', { type: 'runtime' }) : href('activity', { type: 'runtime', q: filter })

  return (
    <div class="page full" style={{ display: 'flex', flexDirection: 'column' }}>
      <h2 class="sr-only">Live logs</h2>
      <div class="toolbar">
        <input class="input grow" type="search" value={filter} spellcheck={false} placeholder="Filter live lines: text or /regex/"
          aria-label="Filter live log lines" onInput={e => setFilter((e.target as HTMLInputElement).value)} />
        <div class="seg" role="group" aria-label="Levels">
          {LEVELS.map(l => (
            <button key={l} aria-pressed={!levels || levels.has(l)} onClick={() => toggleLevel(l)} title={`Show ${l}`}>
              <span class={`log-lvl ${l}`} style={{ width: 'auto' }}>{l.slice(0, 3).toUpperCase()}</span>
            </button>
          ))}
        </div>
        <button class="btn" aria-pressed={paused} onClick={() => (paused ? resume() : setPaused(true))}>
          <Icon name={paused ? 'play' : 'pause'} size="sm" />{paused ? 'Resume' : 'Pause'}
        </button>
        <button class="btn" aria-pressed={settings.wrap} onClick={() => update({ wrap: !settings.wrap })} title="Wrap long lines">Wrap</button>
        <button class="btn" onClick={copyShown} title="Copy every line matching the current filters"><Icon name="copy" size="sm" />{copied ?? 'Copy lines'}</button>
        <button class="btn" onClick={() => { live.clear(); setExpanded(new Set()) }} title="Clear this view (does not delete history)"><Icon name="x" size="sm" />Clear</button>
        <a class="btn" href={historyHref} title="Search durable history (older than this live window)"><Icon name="search" size="sm" />Search history</a>
      </div>
      {live.missed > 0 && (
        <Callout tone="warn">This view fell behind and the daemon dropped {count(live.missed)} live line{live.missed === 1 ? '' : 's'} for it; it caught up from the daemon buffer, which may not hold all of them. Durable history, if enabled, is unaffected unless storage also reports loss (see Integrations).</Callout>
      )}
      {live.lostOnCatchUp > 0 && (
        <Callout tone="warn">{count(live.lostOnCatchUp)} line{live.lostOnCatchUp === 1 ? '' : 's'} left the daemon's live buffer while this page was disconnected or asleep and could not be shown here. If durable history is enabled, they are in Activity.</Callout>
      )}
      {live.restarts > 0 && <Callout tone="info">The daemon restarted {live.restarts === 1 ? 'once' : `${live.restarts} times`} while this page was open; the view was reset to the new process's buffer.</Callout>}
      <div class="logs-layout">
        <aside class="card source-panel" aria-label="Sources" style={{ padding: '8px', overflow: 'auto' }}>
          <div class="nav-section" style={{ paddingTop: '4px' }}>Sources</div>
          <div class="source-list">
            <button class="source-row" aria-pressed={sources === null} onClick={() => setSources(null)}>
              <span class="source-dot" style={{ background: 'var(--muted)' }} />all<span class="n">{count(records.length)}</span>
            </button>
            {[...sourceCounts.entries()].map(([name, n]) => (
              <button key={name} class="source-row" aria-pressed={sources === null || sources.has(name)} onClick={() => toggleSource(name)} title={name}>
                <span class="source-dot" style={{ background: sourceColor(name) }} />
                <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{name}</span>
                <span class="n">{count(n)}</span>
              </button>
            ))}
          </div>
        </aside>
        <div class="logs-panel">
          <div class={`feed${settings.wrap ? ' wrap' : ''}`}>
            {records.length === 0 ? (
              <Empty icon="logs" title={live.status === 'connected' ? 'Waiting for log lines' : 'Connecting to the daemon…'}>
                Runtime output from the sources in <code>tailflow.toml</code> (or <code>--docker</code>, stdin, files) streams here live.
              </Empty>
            ) : shown.length === 0 ? (
              <Empty icon="filter" title="No live lines match">Older lines may still match in durable history — use Search history.</Empty>
            ) : (
              <VirtualList
                items={shown}
                keyOf={r => r.seq}
                estimate={20}
                overscan={20}
                follow={follow && !paused}
                noWrap={!settings.wrap}
                label="Live log lines"
                handle={h => { list.current = h }}
                onFollowChange={atBottom => { if (!paused && atBottom !== follow) setFollow(atBottom) }}
                render={r => <LogLine r={r} tz={settings.tz} expanded={expanded.has(r.seq)} onToggle={() => setExpanded(prev => {
                  const next = new Set(prev)
                  if (next.has(r.seq)) next.delete(r.seq); else next.add(r.seq)
                  return next
                })} />}
              />
            )}
            {(paused || !follow) && records.length > 0 && (
              <button class="btn primary feed-pill" onClick={resume}>
                <Icon name="down" size="sm" />
                {live.queued > 0 ? `${count(live.queued)} new line${live.queued === 1 ? '' : 's'} — resume` : 'Resume live'}
              </button>
            )}
          </div>
          <div class="note" style={{ marginTop: '6px' }}>
            {count(shown.length)} of {count(records.length)} live lines shown (the newest {count(MAX_RECORDS)} are kept here; older lines are in durable history when it is enabled).
            {live.droppedWhileFrozen > 0 && ` ${count(live.droppedWhileFrozen)} lines arrived while paused and were not kept here; with durable history enabled, Search history finds them.`}
          </div>
        </div>
      </div>
    </div>
  )
}

function LogLine({ r, tz, expanded, onToggle }: { r: LogRecord; tz: 'local' | 'utc'; expanded: boolean; onToggle: () => void }) {
  const json = useMemo(() => parseJson(r.payload), [r.payload])
  return (
    <div class={`log-row${r.level === 'error' ? ' error' : ''}`}>
      <span class="log-ts">{clock(r.timestamp, tz, true)}</span>
      <span class="log-src" style={{ color: sourceColor(r.source) }} title={r.source}>{r.source}</span>
      <span class={`log-lvl ${r.level}`}>{r.level === 'unknown' ? '' : r.level.slice(0, 5).toUpperCase()}</span>
      <span class="log-msg">
        {json !== null ? (
          <>
            <button class="json-toggle" onClick={onToggle} aria-expanded={expanded} aria-label={expanded ? 'Collapse JSON' : 'Expand JSON'}>{expanded ? '▾' : '▸'}</button>
            {expanded ? <pre class="json-block">{JSON.stringify(json, null, 2)}</pre> : r.payload}
          </>
        ) : r.payload}
      </span>
    </div>
  )
}
