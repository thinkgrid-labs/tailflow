import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { matchesQuery } from '../lib/liveFilters.ts'
import { api } from '../lib/api.ts'
import { navigate, useAsync, useSettings, useDebounced } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { buildItems, packRows, sessionBars, tickStep, zoom, type TimelineItem, type SessionBar } from '../lib/timeline.ts'
import { clock, dateTime, duration, count, projectName } from '../lib/format.ts'
import { agentLabel } from '../lib/events.ts'
import type { StoredEvent } from '../lib/types.ts'
import { Callout, Card, CopyButton, ErrorState, Segmented } from '../components/ui'
import { Inspector } from '../components/Inspector'
import { Icon } from '../components/icons'

const AGENT_TYPES = 'agent.tool.*,command.*,file.*,agent.subagent.*,agent.session.*,agent.turn.failed'
const MAX_AGENT_EVENTS = 3_000
const PAGE = 1_000
const ROW = 14
const LANE_PAD = 10
const AXIS = 26
const PRESETS = [
  { value: '15m', label: '15m', ms: 15 * 60e3 },
  { value: '1h', label: '1h', ms: 3600e3 },
  { value: '6h', label: '6h', ms: 6 * 3600e3 },
  { value: '24h', label: '24h', ms: 86400e3 },
  { value: '7d', label: '7d', ms: 7 * 86400e3 },
]

interface Data {
  items: TimelineItem[]
  runtime: TimelineItem[]
  bars: SessionBar[]
  /** More agent events than drawn: the earliest MAX_AGENT_EVENTS are shown. */
  agentTruncated: boolean
  /** More runtime markers than drawn: the newest 500 are shown. */
  runtimeTruncated: boolean
  /** More than 200 sessions overlap: some session bars are missing. */
  sessionsTruncated: boolean
  events: number
}

export function Timeline({ params }: { params: URLSearchParams }) {
  const p = toRecord(params)
  const { settings } = useSettings()
  // Live mode keeps a fixed span ending now (`span`); a fixed window has
  // `from` and `to`. A legacy `from` without `to` becomes a span once.
  const live = !p.to
  const [now, setNow] = useState(Date.now())
  useEffect(() => {
    if (!live) return
    const t = setInterval(() => { if (document.visibilityState === 'visible') setNow(Date.now()) }, 15_000)
    return () => clearInterval(t)
  }, [live])
  const span = Math.max(5e3, Number(p.span) || (p.from && !p.to ? now - Number(p.from) : 3600e3))
  const urlTo = p.to ? Number(p.to) : now
  const urlFrom = p.to && p.from ? Number(p.from) : urlTo - span
  // The window being shown; follows the URL, moves locally while dragging.
  const [view, setView] = useState<[number, number]>([urlFrom, urlTo])
  useEffect(() => setView([urlFrom, urlTo]), [urlFrom, urlTo])
  const [t0, t1] = view
  const fetchWin = useDebounced(view, 250)
  const selected = p.event ? Number(p.event) : null
  const showRuntime = p.runtime !== '0'

  const commit = (a: number, b: number) => navigate(href('timeline', withParams(params, { from: Math.round(a), to: Math.round(b), span: null, event: p.event ?? null })), true)
  const goLive = (ms: number) => navigate(href('timeline', withParams(params, { span: Math.round(ms), from: null, to: null, event: null })), true)
  const set = (c: Record<string, string | number | null>) => navigate(href('timeline', withParams(params, c)), true)

  const data = useAsync<Data>(async s => {
    const [a, b] = fetchWin
    const since = new Date(a).toISOString()
    const until = new Date(b).toISOString()
    const sessionsP = api.sessions({ since, until, agent: p.agent, project: p.project, limit: 200 }, s)
    const runtimeP = showRuntime
      ? api.events({ type: 'runtime.error,runtime.warning', since, until, project: p.project, order: 'desc', limit: 500 }, s)
      : null
    const events: StoredEvent[] = []
    let after: number | null = null
    let agentTruncated = false
    for (;;) {
      const page = await api.events({ type: AGENT_TYPES, since, until, agent: p.agent, project: p.project, order: 'asc', limit: PAGE, after }, s)
      events.push(...page.events)
      if (!page.has_more || page.next_cursor === null) break
      if (events.length >= MAX_AGENT_EVENTS) { agentTruncated = true; break }
      after = page.next_cursor
    }
    const [sessions, runtime] = await Promise.all([sessionsP, runtimeP])
    return {
      items: buildItems(events),
      runtime: runtime ? buildItems(runtime.events) : [],
      bars: sessionBars(sessions.sessions, a, b),
      agentTruncated,
      runtimeTruncated: runtime?.has_more ?? false,
      sessionsTruncated: sessions.has_more,
      events: events.length,
    }
  }, [fetchWin[0], fetchWin[1], p.agent, p.project, showRuntime], live ? 15_000 : undefined,
  // Following "now": committed agent events extend the view (a window
  // reload is up to 3,000 events, so at most every 3 s).
  live ? e => matchesQuery(e, { agent: p.agent, project: p.project }) : undefined, 3_000)

  return (
    <div class="page" style={{ maxWidth: 'none' }}>
      <div class="page-head">
        <div>
          <h2>Timeline</h2>
          <p>Agents side by side, with runtime warnings and errors underneath. Drag to pan · Shift+drag to zoom to a range · Ctrl/⌘+scroll to zoom.</p>
        </div>
        <CopyButton text={location.href} label="Copy view URL" />
      </div>
      <div class="toolbar">
        <Segmented label="Window" value={live ? (PRESETS.find(x => Math.abs(x.ms - (t1 - t0)) < 1000)?.value ?? '') : ''}
          options={PRESETS} onChange={v => goLive(PRESETS.find(x => x.value === v)?.ms ?? 3600e3)} />
        <button class="btn icon-only" aria-label="Pan earlier" title="Pan earlier (←)" onClick={() => { const d = (t1 - t0) / 3; commit(t0 - d, t1 - d) }}><Icon name="left" /></button>
        <button class="btn icon-only" aria-label="Pan later" title="Pan later (→)" onClick={() => { const d = (t1 - t0) / 3; commit(t0 + d, t1 + d) }}><Icon name="right" /></button>
        <button class="btn icon-only" aria-label="Zoom in" title="Zoom in (+)" onClick={() => { const [a, b] = zoom(t0, t1, 0.5, (t0 + t1) / 2); commit(a, b) }}><Icon name="zoomIn" /></button>
        <button class="btn icon-only" aria-label="Zoom out" title="Zoom out (−)" onClick={() => { const [a, b] = zoom(t0, t1, 2, (t0 + t1) / 2); commit(a, b) }}><Icon name="zoomOut" /></button>
        <button class="btn" aria-pressed={live} onClick={() => goLive(t1 - t0)}>
          <Icon name="play2" size="sm" />{live ? 'Following now' : 'Jump to now'}
        </button>
        <select class="select" aria-label="Agent" value={p.agent ?? ''} onChange={e => set({ agent: (e.target as HTMLSelectElement).value || null })}>
          <option value="">All agents</option><option value="claude-code">Claude Code</option><option value="codex">Codex</option>
        </select>
        <label class="btn"><input type="checkbox" checked={showRuntime} onChange={e => set({ runtime: (e.target as HTMLInputElement).checked ? null : '0' })} /> Runtime</label>
        {p.project && <span class="chip"><span>project: {projectName(p.project)}</span><button aria-label="Remove project filter" onClick={() => set({ project: null })}><Icon name="x" size="sm" /></button></span>}
      </div>
      <div class="note">{dateTime(new Date(t0).toISOString(), settings.tz)} → {dateTime(new Date(t1).toISOString(), settings.tz)} · {duration(t1 - t0)}{data.data && ` · ${count(data.data.events)} agent events`}</div>
      {data.data && (data.data.agentTruncated || data.data.runtimeTruncated || data.data.sessionsTruncated) && (
        <Callout tone="warn">
          Not everything in this window is drawn:
          {data.data.agentTruncated && ` only the earliest ${count(MAX_AGENT_EVENTS)} agent events;`}
          {data.data.runtimeTruncated && ' only the newest 500 runtime markers;'}
          {data.data.sessionsTruncated && ' only the 200 most recently active sessions have bars;'}
          {' '}zoom in to see everything.
        </Callout>
      )}
      <div class={selected !== null ? 'with-inspector' : ''} style={{ display: selected !== null ? undefined : 'block' }}>
        <div>
          {data.error && !data.data ? <Card><ErrorState error={data.error} onRetry={data.reload} /></Card>
            : <Chart data={data.data} loading={data.loading} t0={t0} t1={t1} selected={selected}
                onView={(a, b) => setView([a, b])} onCommit={commit}
                onSelect={seq => set({ event: seq })} onSession={id => navigate(href('session', {}, id))} />}
          <Legend />
        </div>
        {selected !== null && <Inspector seq={selected} onClose={() => set({ event: null })} onSelect={seq => set({ event: seq })} />}
      </div>
    </div>
  )
}

function Legend() {
  const sw = (cls: string) => <svg width="18" height="10" aria-hidden="true" class="tl" style={{ border: 'none', background: 'none', display: 'inline-block', verticalAlign: '-1px', marginRight: '5px' }}><rect class={cls} x="0" y="1" width="18" height="8" rx="2" /></svg>
  return (
    <div class="legend" style={{ marginTop: '10px' }}>
      <span>{sw('k-tool')}Tool call</span>
      <span>{sw('k-command')}Command</span>
      <span>{sw('k-file')}Reported file op</span>
      <span>{sw('k-subagent')}Subagent</span>
      <span>{sw('k-failure')}Failure</span>
      <span>{sw('session')}Session (observed span)</span>
      <span class="note">Bars only where a start and end, or a duration, were reported; everything else is a point. Points with an amber outline are starts whose end is outside this window.</span>
    </div>
  )
}

interface Lane { key: string; label: string; sessionRows: number; itemRows: number; y: number; h: number }

function Chart({ data, loading, t0, t1, selected, onView, onCommit, onSelect, onSession }: {
  data: Data | null; loading: boolean; t0: number; t1: number; selected: number | null
  onView: (a: number, b: number) => void; onCommit: (a: number, b: number) => void
  onSelect: (seq: number) => void; onSession: (id: string) => void
}) {
  const { settings } = useSettings()
  const box = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(900)
  const [tip, setTip] = useState<{ x: number; y: number; text: string } | null>(null)
  const [announce, setAnnounce] = useState('')
  const drag = useRef<{ x: number; t0: number; t1: number; brush: boolean; moved: boolean } | null>(null)
  const [brush, setBrush] = useState<[number, number] | null>(null)

  useEffect(() => {
    const el = box.current
    if (!el) return
    const ro = new ResizeObserver(() => setWidth(el.clientWidth))
    ro.observe(el)
    setWidth(el.clientWidth)
    return () => ro.disconnect()
  }, [])

  const gutter = width < 600 ? 84 : 132
  const plot = Math.max(50, width - gutter - 12)
  const x = (t: number) => gutter + ((t - t0) / (t1 - t0)) * plot
  const tAt = (px: number) => t0 + ((px - gutter) / plot) * (t1 - t0)

  // Lanes and row packing (recomputed only when data or zoom level change).
  const layout = useMemo(() => {
    if (!data) return null
    const span = t1 - t0
    const minGap = (span / plot) * 6
    const items = data.items.map(i => ({ ...i }))
    const runtime = data.runtime.map(i => ({ ...i, lane: 'runtime' }))
    const rows = packRows(items, minGap)
    const lanesKeys = new Set<string>(['claude-code', 'codex'])
    for (const i of items) lanesKeys.add(i.lane)
    for (const b of data.bars) lanesKeys.add(b.lane)
    const lanes: Lane[] = []
    let y = AXIS
    for (const key of lanesKeys) {
      if (key === 'runtime') continue
      const sessionRows = Math.max(1, ...data.bars.filter(b => b.lane === key).map(b => b.row + 1))
      const itemRows = Math.max(1, rows.get(key) ?? 1)
      const h = LANE_PAD * 2 + (sessionRows + itemRows) * ROW + 4
      lanes.push({ key, label: agentLabel(key), sessionRows, itemRows, y, h })
      y += h
    }
    // The runtime lane is always drawn, so "no runtime errors" is visible.
    const rh = LANE_PAD * 2 + ROW
    lanes.push({ key: 'runtime', label: 'Runtime', sessionRows: 0, itemRows: 1, y, h: rh })
    y += rh
    return { items, runtime, lanes, height: y + 4 }
  }, [data, Math.round((t1 - t0) / 1000), plot])

  const height = layout?.height ?? 200
  const step = tickStep(t1 - t0, Math.max(3, Math.floor(plot / 110)))
  const ticks: number[] = []
  for (let t = Math.ceil(t0 / step) * step; t <= t1; t += step) ticks.push(t)
  const multiDay = t1 - t0 > 86400e3
  const nowX = Date.now() >= t0 && Date.now() <= t1 ? x(Date.now()) : null

  const onPointerDown = (e: PointerEvent) => {
    if ((e.target as Element).closest('.item, .session')) return
    ;(e.currentTarget as Element).setPointerCapture(e.pointerId)
    const rect = box.current!.getBoundingClientRect()
    drag.current = { x: e.clientX - rect.left, t0, t1, brush: e.shiftKey, moved: false }
    if (e.shiftKey) setBrush([e.clientX - rect.left, e.clientX - rect.left])
  }
  const onPointerMove = (e: PointerEvent) => {
    const d = drag.current
    if (!d) return
    const rect = box.current!.getBoundingClientRect()
    const px = e.clientX - rect.left
    if (Math.abs(px - d.x) > 3) d.moved = true
    if (d.brush) setBrush([d.x, px])
    else {
      const dt = ((px - d.x) / plot) * (d.t1 - d.t0)
      onView(d.t0 - dt, d.t1 - dt)
    }
  }
  const onPointerUp = () => {
    const d = drag.current
    drag.current = null
    if (!d) return
    if (d.brush && brush) {
      const a = tAt(Math.min(brush[0], brush[1]))
      const b = tAt(Math.max(brush[0], brush[1]))
      setBrush(null)
      if (b - a > 1000) onCommit(a, b)
    } else if (d.moved) {
      onCommit(t0, t1)
    }
  }
  const onWheel = (e: WheelEvent) => {
    if (!e.ctrlKey && !e.metaKey) return
    e.preventDefault()
    const rect = box.current!.getBoundingClientRect()
    const [a, b] = zoom(t0, t1, e.deltaY > 0 ? 1.25 : 0.8, tAt(e.clientX - rect.left))
    onCommit(a, b)
  }
  const onKey = (e: KeyboardEvent) => {
    const span = t1 - t0
    if (e.key === '+' || e.key === '=') { const [a, b] = zoom(t0, t1, 0.5, (t0 + t1) / 2); onCommit(a, b) }
    else if (e.key === '-' || e.key === '_') { const [a, b] = zoom(t0, t1, 2, (t0 + t1) / 2); onCommit(a, b) }
    else if (e.key === 'ArrowLeft') onCommit(t0 - span / 4, t1 - span / 4)
    else if (e.key === 'ArrowRight') onCommit(t0 + span / 4, t1 + span / 4)
    else if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Home' || e.key === 'End') {
      // Step through the visible items in time order; each opens in the
      // Inspector and is announced.
      const order = [
        ...visibleItems.map(i => ({ seq: i.seq, start: i.start, text: `${i.label}${i.failed ? ', failed' : ''}` })),
        ...visibleRuntime.map(i => ({ seq: i.seq, start: i.start, text: `${i.failed ? 'Runtime error' : 'Runtime warning'}: ${i.label.slice(0, 120)}` })),
      ].sort((a, b) => a.start - b.start || a.seq - b.seq)
      if (order.length === 0) { setAnnounce('No items in this window'); e.preventDefault(); return }
      const at = order.findIndex(o => o.seq === selected)
      const next = e.key === 'Home' ? 0
        : e.key === 'End' ? order.length - 1
        : at < 0 ? (e.key === 'ArrowDown' ? 0 : order.length - 1)
        : Math.min(order.length - 1, Math.max(0, at + (e.key === 'ArrowDown' ? 1 : -1)))
      const o = order[next]
      onSelect(o.seq)
      setAnnounce(`${next + 1} of ${order.length}: ${o.text}, ${clock(new Date(o.start).toISOString(), settings.tz, true)}`)
    }
    else return
    e.preventDefault()
  }

  const showTip = (e: MouseEvent, text: string) => {
    const rect = box.current!.getBoundingClientRect()
    setTip({ x: Math.min(e.clientX - rect.left + 12, width - 240), y: e.clientY - rect.top + 12, text })
  }
  const itemTip = (i: TimelineItem) => {
    const when = i.point ? clock(new Date(i.start).toISOString(), settings.tz, true)
      : `${clock(new Date(i.start).toISOString(), settings.tz, true)} → ${clock(new Date(i.end).toISOString(), settings.tz, true)} (${duration(i.end - i.start)}, ${i.basis === 'start-end' ? 'start and end reported' : 'duration reported'})`
    return `${i.label}${i.failed ? ' — failed' : ''}${i.openEnded ? ' — end not in this window' : ''}\n${when}`
  }

  const visibleItems = (layout?.items ?? []).filter(i => i.end >= t0 && i.start <= t1)
  const visibleRuntime = (layout?.runtime ?? []).filter(i => i.start >= t0 && i.start <= t1)

  return (
    <div class="tl" ref={box} tabIndex={0} role="group" aria-roledescription="timeline"
      aria-label={`Timeline from ${dateTime(new Date(t0).toISOString(), settings.tz)} to ${dateTime(new Date(t1).toISOString(), settings.tz)}. ${visibleItems.length} agent operations, ${visibleRuntime.length} runtime markers. Use plus and minus to zoom, left and right arrows to pan, up and down arrows to step through items.`}
      onKeyDown={onKey} onWheel={onWheel}>
      {loading && <div class="loading-bar" aria-hidden="true" />}
      <div class="sr-only" aria-live="polite">{announce}</div>
      <svg height={height} onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={onPointerUp} onPointerLeave={() => setTip(null)}>
        <g class="axis">
          {ticks.map(t => (
            <g key={t}>
              <line class="grid-line" x1={x(t)} x2={x(t)} y1={AXIS - 6} y2={height} />
              <text x={x(t) + 4} y={16}>{multiDay ? `${new Date(t).toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: settings.tz === 'utc' ? 'UTC' : undefined })} ` : ''}{clock(new Date(t).toISOString(), settings.tz).slice(0, step < 60e3 ? 8 : 5)}</text>
            </g>
          ))}
        </g>
        {layout?.lanes.map((lane, li) => (
          <g key={lane.key}>
            {li % 2 === 0 && <rect class="lane-bg" x={0} y={lane.y} width={width} height={lane.h} opacity={0.5} />}
            <text class="lane-label" x={10} y={lane.y + LANE_PAD + 10}>{lane.label}</text>
            {lane.key !== 'runtime' && <text class="lane-sub" x={10} y={lane.y + LANE_PAD + 24}>
              {(data?.bars.filter(b => b.lane === lane.key).length ?? 0)} session(s)
            </text>}
          </g>
        ))}
        <clipPath id="plot"><rect x={gutter} y={0} width={plot} height={height} /></clipPath>
        <g clip-path="url(#plot)">
          {layout && data?.bars.map(b => {
            const lane = layout.lanes.find(l => l.key === b.lane)
            if (!lane) return null
            const x0 = Math.max(gutter - 2, x(b.start))
            const x1 = Math.min(gutter + plot + 2, x(b.end))
            const y = lane.y + LANE_PAD + b.row * ROW
            return (
              <g key={b.sessionId} class="session-g">
                <rect class={`session ${b.status}`} x={x0} y={y} width={Math.max(3, x1 - x0)} height={ROW - 3} rx={3}
                  onClick={() => onSession(b.sessionId)}
                  onMouseMove={e => showTip(e, `Session ${b.sessionId}\n${projectName(b.label)} · ${b.status === 'unterminated' ? 'no end observed' : b.status}\nObserved activity ${duration(b.end - b.start)} (click to open)`)} />
                {x1 - x0 > 80 && <text class="session-label" x={x0 + 6} y={y + 9}>{projectName(b.label)}</text>}
              </g>
            )
          })}
          {layout && visibleItems.map(i => {
            const lane = layout.lanes.find(l => l.key === i.lane)
            if (!lane) return null
            const y = lane.y + LANE_PAD + (lane.sessionRows + i.row) * ROW + 2
            const cls = `item k-${i.failed ? 'failure' : i.kind}${i.openEnded ? ' open-ended' : ''}${i.seq === selected ? ' sel' : ''}`
            const handlers = {
              onClick: () => onSelect(i.seq),
              onMouseMove: (e: MouseEvent) => showTip(e, itemTip(i)),
            }
            if (i.point) {
              const cx = x(i.start)
              const cy = y + 5
              return <path key={i.id} class={cls} d={`M${cx} ${cy - 5}L${cx + 4.5} ${cy}L${cx} ${cy + 5}L${cx - 4.5} ${cy}Z`} {...handlers} />
            }
            const x0 = x(i.start)
            return <rect key={i.id} class={cls} x={x0} y={y} width={Math.max(2.5, x(i.end) - x0)} height={10} {...handlers} />
          })}
          {layout && visibleRuntime.map(i => {
            const lane = layout.lanes.find(l => l.key === 'runtime')
            if (!lane) return null
            return <rect key={i.id} class={`item ${i.failed ? 'failed' : 'k-runtime'}`} x={x(i.start) - 1} y={lane.y + LANE_PAD} width={2.5} height={ROW}
              onClick={() => onSelect(i.seq)} onMouseMove={e => showTip(e, `${i.failed ? 'Runtime error' : 'Runtime warning'}\n${i.label.slice(0, 160)}\n${clock(new Date(i.start).toISOString(), settings.tz, true)}`)} />
          })}
          {nowX !== null && <line class="now-line" x1={nowX} x2={nowX} y1={AXIS - 6} y2={height}><title>now</title></line>}
          {brush && <rect class="brush" x={Math.min(brush[0], brush[1])} y={AXIS - 6} width={Math.abs(brush[1] - brush[0])} height={height} />}
        </g>
      </svg>
      {data && data.items.length === 0 && data.runtime.length === 0 && data.bars.length === 0 && (
        <div class="empty" style={{ position: 'absolute', inset: `${AXIS}px 0 0 ${gutter}px`, pointerEvents: 'none' }}>
          <Icon name="clock" /><h4>Nothing in this window</h4><p>Zoom out or jump to a time with activity.</p>
        </div>
      )}
      {tip && <div class="tl-tip" style={{ left: `${tip.x}px`, top: `${tip.y}px`, whiteSpace: 'pre-line' }}>{tip.text}</div>}
    </div>
  )
}
