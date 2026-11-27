// Timeline model: turn a bounded window of events into lanes of intervals
// and points. Rules:
//
// - An interval exists only when both ends were reported (a start event and
//   an end event sharing an `operation_id`), or when the end event carries a
//   reported `duration_ms`. Its source is recorded so the UI can say which.
// - Anything else is a point at its own timestamp. A start whose end is not
//   in the window is a point flagged `openEnded`, never a bar stretched to
//   "now".
// - Sessions are drawn over their *observed* activity span (first to last
//   event), labelled as such: TailFlow cannot know when an agent was idle.

import type { StoredEvent, SessionView } from './types.ts'
import { category } from './events.ts'

export type ItemKind = 'tool' | 'command' | 'file' | 'subagent' | 'failure' | 'runtime' | 'session-mark' | 'other'

export interface TimelineItem {
  id: string
  kind: ItemKind
  lane: string
  sessionId: string | null
  start: number
  /** Equal to `start` for points. */
  end: number
  point: boolean
  /** How the interval is known. */
  basis: 'start-end' | 'reported-duration' | 'point'
  openEnded: boolean
  failed: boolean
  label: string
  /** Seq of the event to inspect on click. */
  seq: number
  row: number
}

export interface SessionBar {
  sessionId: string
  lane: string
  start: number
  end: number
  status: SessionView['status']
  label: string
  row: number
}

const ms = (iso: string | null | undefined) => (iso ? Date.parse(iso) : NaN)

function opKey(e: StoredEvent): string | null {
  const op = e.operation_id
  if (!op) return null
  // Codex suffixes per-phase ids; strip the phase so pre/post pair up.
  return `${e.session_id ?? ''}|${op.replace(/:(pre|post)$/, '')}`
}

function kindOf(e: StoredEvent): ItemKind {
  switch (category(e.event_type)) {
    case 'tool': return 'tool'
    case 'command': return 'command'
    case 'file': return 'file'
    case 'subagent': return 'subagent'
    case 'runtime': return 'runtime'
    case 'session': return 'session-mark'
    default: return 'other'
  }
}

function label(e: StoredEvent): string {
  const a = e.attributes ?? {}
  const tool = typeof a['tool.name'] === 'string' ? (a['tool.name'] as string) : null
  const program = typeof a['command.program'] === 'string' ? (a['command.program'] as string) : null
  if (e.event_type.startsWith('command.')) return program ?? 'command'
  if (e.event_type.startsWith('agent.tool.')) return tool ?? 'tool'
  if (e.event_type.startsWith('file.')) {
    const p = (e.path ?? '').replace(/\\/g, '/')
    return p.slice(p.lastIndexOf('/') + 1) || 'file'
  }
  return e.message ?? e.event_type
}

/** Lane name for an event: the agent, or "Runtime" for logs. */
export function laneOf(e: StoredEvent): string {
  return e.agent_name ?? 'runtime'
}

/**
 * Build items from events in any order. Start/end pairs are joined by
 * operation id; durations are only ever those the agent reported.
 */
export function buildItems(events: readonly StoredEvent[]): TimelineItem[] {
  const items: TimelineItem[] = []
  const starts = new Map<string, StoredEvent>()
  const ends = new Map<string, StoredEvent>()
  for (const e of events) {
    const k = opKey(e)
    const isStart = e.event_type === 'agent.tool.started' || e.event_type === 'command.started'
    const isEnd = /^(agent\.tool|command)\.(completed|failed)$/.test(e.event_type)
    if (k && isStart) starts.set(`${k}|${kindOf(e)}`, e)
    else if (k && isEnd) ends.set(`${k}|${kindOf(e)}`, e)
    else {
      const t = ms(e.timestamp)
      if (Number.isNaN(t)) continue
      const failed = e.status === 'failed' || e.event_type === 'runtime.error'
      items.push({
        id: `e${e.seq}`, kind: failed && kindOf(e) !== 'runtime' ? 'failure' : kindOf(e),
        lane: laneOf(e), sessionId: e.session_id ?? null, start: t, end: t, point: true,
        basis: 'point', openEnded: false, failed, label: label(e), seq: e.seq, row: 0,
      })
    }
  }
  for (const [key, end] of ends) {
    const t1 = ms(end.timestamp)
    if (Number.isNaN(t1)) continue
    const start = starts.get(key)
    starts.delete(key)
    const failed = end.status === 'failed' || end.status === 'cancelled'
    const base = {
      id: `e${end.seq}`, kind: kindOf(end), lane: laneOf(end), sessionId: end.session_id ?? null,
      failed, label: label(end), seq: end.seq, row: 0, openEnded: false,
    }
    const t0 = start ? ms(start.timestamp) : NaN
    if (!Number.isNaN(t0) && t0 <= t1) {
      items.push({ ...base, start: t0, end: t1, point: t0 === t1, basis: 'start-end' })
    } else if (typeof end.duration_ms === 'number' && end.duration_ms > 0) {
      items.push({ ...base, start: t1 - end.duration_ms, end: t1, point: false, basis: 'reported-duration' })
    } else {
      items.push({ ...base, start: t1, end: t1, point: true, basis: 'point' })
    }
  }
  for (const start of starts.values()) {
    const t = ms(start.timestamp)
    if (Number.isNaN(t)) continue
    items.push({
      id: `e${start.seq}`, kind: kindOf(start), lane: laneOf(start), sessionId: start.session_id ?? null,
      start: t, end: t, point: true, basis: 'point', openEnded: true, failed: false,
      label: label(start), seq: start.seq, row: 0,
    })
  }
  return items.sort((a, b) => a.start - b.start || a.seq - b.seq)
}

/**
 * Greedy row packing inside each lane: an item goes on the first row whose
 * last item ended before it starts (with `minGapMs` so points stay apart).
 * Returns rows per lane.
 */
export function packRows(items: TimelineItem[], minGapMs: number, maxRows = 6): Map<string, number> {
  const rowEnds = new Map<string, number[]>()
  for (const it of items) {
    const ends = rowEnds.get(it.lane) ?? []
    let row = ends.findIndex(e => e + minGapMs <= it.start)
    if (row === -1) {
      if (ends.length < maxRows) {
        row = ends.length
        ends.push(it.end)
      } else {
        // Out of rows: stack on the row that frees up first.
        row = ends.indexOf(Math.min(...ends))
        ends[row] = Math.max(ends[row], it.end)
      }
    } else {
      ends[row] = Math.max(it.end, it.start + minGapMs)
    }
    it.row = row
    rowEnds.set(it.lane, ends)
  }
  const out = new Map<string, number>()
  for (const [lane, ends] of rowEnds) out.set(lane, ends.length)
  return out
}

/** Sessions overlapping `[t0, t1]`, over their observed span, packed in rows per lane. */
export function sessionBars(sessions: readonly SessionView[], t0: number, t1: number): SessionBar[] {
  const bars: SessionBar[] = []
  for (const s of sessions) {
    const a = ms(s.first_event_at)
    const b = ms(s.last_event_at)
    if (Number.isNaN(a) || Number.isNaN(b) || b < t0 || a > t1) continue
    bars.push({
      sessionId: s.session_id, lane: s.agent_name, start: a, end: Math.max(a, b),
      status: s.status, label: s.project_id ?? s.session_id, row: 0,
    })
  }
  bars.sort((x, y) => x.start - y.start)
  const rows = new Map<string, number[]>()
  for (const bar of bars) {
    const ends = rows.get(bar.lane) ?? []
    let r = ends.findIndex(e => e < bar.start)
    if (r === -1) { r = ends.length; ends.push(bar.end) } else ends[r] = bar.end
    bar.row = r
    rows.set(bar.lane, ends)
  }
  return bars
}

/** Nice tick step (ms) for a span, targeting ~`target` ticks. */
export function tickStep(spanMs: number, target = 6): number {
  const steps = [
    1e3, 2e3, 5e3, 10e3, 15e3, 30e3, 60e3, 2 * 60e3, 5 * 60e3, 10 * 60e3, 15 * 60e3, 30 * 60e3,
    3600e3, 2 * 3600e3, 3 * 3600e3, 6 * 3600e3, 12 * 3600e3, 24 * 3600e3, 2 * 86400e3, 7 * 86400e3,
  ]
  const ideal = spanMs / Math.max(1, target)
  return steps.find(s => s >= ideal) ?? steps[steps.length - 1]
}

/** Zoom `[t0, t1]` by `factor` (<1 zooms in) around `anchor`, clamped to [minSpan, maxSpan]. */
export function zoom(t0: number, t1: number, factor: number, anchor: number, minSpan = 5e3, maxSpan = 31 * 86400e3): [number, number] {
  const span = Math.min(maxSpan, Math.max(minSpan, (t1 - t0) * factor))
  const ratio = t1 > t0 ? (anchor - t0) / (t1 - t0) : 0.5
  const a = anchor - span * Math.min(1, Math.max(0, ratio))
  return [Math.round(a), Math.round(a + span)]
}
