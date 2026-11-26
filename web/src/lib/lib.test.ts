// Unit tests for the dashboard's pure modules. Run with Node's built-in
// runner (no dependencies): `npm test`.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { append, boundPending, compileFilter, mergeBySeq, MAX_RECORDS } from './logStore.ts'
import { HeightIndex } from './virtual.ts'
import { buildItems, packRows, sessionBars, tickStep, zoom } from './timeline.ts'
import { parseHash, href, withParams } from './router.ts'
import { clock, dateTime, duration, relative, shortPath, relativeTo, compact } from './format.ts'
import { sanitize, DEFAULT_SETTINGS } from './settings.ts'
import { statusOf, summary, agentLabel, isFailure } from './events.ts'
import type { LogRecord, StoredEvent, SessionView } from './types.ts'

const rec = (seq: number, payload = `line ${seq}`): LogRecord =>
  ({ seq, timestamp: '2026-11-25T10:00:00Z', source: 'api', level: 'info', payload })

function ev(seq: number, type: string, extra: Partial<StoredEvent> = {}): StoredEvent {
  return {
    seq, event_id: `e${seq}`, schema_version: 1, timestamp: '2026-11-25T10:00:00.000Z',
    observed_at: '2026-11-25T10:00:00.000Z', event_type: type, provenance: 'agent_reported',
    source_type: 'agent_hook', source_id: 'claude-code', agent_name: 'claude-code',
    session_id: 'claude-code:s1', severity: 'info', ...extra,
  }
}

// ── log store ───────────────────────────────────────────────────────────────

test('live batches append in order without re-sorting', () => {
  const a = append([], [rec(1), rec(2)])
  const b = append(a.records, [rec(3)])
  assert.deepEqual(b.records.map(r => r.seq), [1, 2, 3])
  assert.equal(b.trimmed, 0)
})

test('overlapping catch-up and live records are deduplicated by seq', () => {
  const live = append([], [rec(5), rec(6)]).records
  const merged = append(live, [rec(3), rec(4), rec(5), rec(6)]).records
  assert.deepEqual(merged.map(r => r.seq), [3, 4, 5, 6])
  assert.deepEqual(mergeBySeq([rec(2), rec(1)], [rec(1, 'newer')]).map(r => r.payload), ['newer', 'line 2'])
})

test('out-of-order delivery ends up ascending', () => {
  const r = append([rec(1), rec(4)], [rec(3), rec(2)]).records
  assert.deepEqual(r.map(x => x.seq), [1, 2, 3, 4])
})

test('cap trims the oldest and reports how many rendered rows left', () => {
  const start = Array.from({ length: 10 }, (_, i) => rec(i + 1))
  const r = append(start, [rec(11), rec(12), rec(13)], 10)
  assert.equal(r.records.length, 10)
  assert.equal(r.records[0].seq, 4)
  assert.equal(r.trimmed, 3)
  // A burst bigger than the cap trims every previously rendered row.
  const big = Array.from({ length: 25 }, (_, i) => rec(100 + i))
  const r2 = append(start, big, 10)
  assert.equal(r2.records[0].seq, 115)
  assert.equal(r2.trimmed, 10)
})

test('pending queue is bounded for hidden tabs', () => {
  const pending = Array.from({ length: MAX_RECORDS + 1234 }, (_, i) => rec(i))
  assert.equal(boundPending(pending), 1234)
  assert.equal(pending.length, MAX_RECORDS)
  assert.equal(pending[0].seq, 1234, 'the newest records are kept')
})

test('filter: regex with slashes, substring otherwise, invalid regex falls back', () => {
  const f = compileFilter('/err(or)?/')!
  assert.ok(f(rec(1, 'ERROR boom')))
  assert.ok(!f(rec(1, 'fine')))
  const s = compileFilter('Boom')!
  assert.ok(s(rec(1, 'big boom')))
  const bad = compileFilter('/[/')!
  assert.ok(bad(rec(1, 'a /[/ b')), 'invalid regex is a literal substring')
  assert.equal(compileFilter('  '), null)
})

// ── virtualization ──────────────────────────────────────────────────────────

test('height index: estimates, measurements and ranges', () => {
  const h = new HeightIndex(20)
  const keys = Array.from({ length: 1000 }, (_, i) => i)
  h.setKeys(keys)
  assert.equal(h.total(), 20_000)
  assert.deepEqual(h.range(0, 100, 0), { start: 0, end: 6 })
  assert.deepEqual(h.range(10_000, 100, 5), { start: 495, end: 511 })
  assert.ok(h.measure(0, 120), 'an expanded row changes layout')
  assert.ok(!h.measure(0, 120.2), 'sub-pixel jitter does not')
  assert.equal(h.total(), 20_100)
  assert.equal(h.offsetOf(1), 120)
  assert.equal(h.indexAt(119), 0)
  assert.equal(h.indexAt(120), 1)
  assert.equal(h.heightOfFirst(2), 140)
})

test('height index keeps measurements by key across trimming', () => {
  const h = new HeightIndex(10)
  h.setKeys([1, 2, 3])
  h.measure(3, 50)
  h.setKeys([3, 4])
  assert.equal(h.offsetOf(1), 50)
  assert.deepEqual(new HeightIndex(10).range(0, 100, 3), { start: 0, end: 0 })
})

// ── timeline ────────────────────────────────────────────────────────────────

const at = (s: number) => new Date(Date.UTC(2026, 10, 25, 10, 0, s)).toISOString()

test('intervals need both reported ends or a reported duration', () => {
  const items = buildItems([
    ev(1, 'agent.tool.started', { timestamp: at(0), operation_id: 'tool:a' }),
    ev(2, 'agent.tool.completed', { timestamp: at(5), operation_id: 'tool:a', status: 'succeeded' }),
    ev(3, 'agent.tool.completed', { timestamp: at(20), operation_id: 'tool:b', duration_ms: 4000 }),
    ev(4, 'agent.tool.completed', { timestamp: at(30), operation_id: 'tool:c' }),
    ev(5, 'agent.tool.started', { timestamp: at(40), operation_id: 'tool:d' }),
  ])
  const byId = Object.fromEntries(items.map(i => [i.seq, i]))
  assert.equal(byId[2].basis, 'start-end')
  assert.equal(byId[2].end - byId[2].start, 5000)
  assert.equal(byId[3].basis, 'reported-duration')
  assert.equal(byId[3].end - byId[3].start, 4000)
  assert.equal(byId[4].basis, 'point', 'no start, no duration: a point, never a guessed bar')
  assert.ok(byId[4].point)
  assert.ok(byId[5].openEnded, 'an unmatched start is a flagged point, not a bar to now')
  assert.equal(byId[5].end, byId[5].start)
})

test('tool and command intervals with the same id stay separate; failures flagged', () => {
  const items = buildItems([
    ev(1, 'agent.tool.started', { timestamp: at(0), operation_id: 'tool:x' }),
    ev(2, 'command.started', { timestamp: at(0), operation_id: 'cmd:x', parent_operation_id: 'tool:x' }),
    ev(3, 'agent.tool.failed', { timestamp: at(3), operation_id: 'tool:x', status: 'failed' }),
    ev(4, 'command.failed', { timestamp: at(3), operation_id: 'cmd:x', status: 'failed' }),
    ev(5, 'runtime.error', { timestamp: at(4), agent_name: undefined, provenance: 'runtime_observed' }),
  ])
  assert.equal(items.length, 3)
  assert.ok(items.filter(i => i.kind !== 'runtime').every(i => i.failed && i.basis === 'start-end'))
  const rt = items.find(i => i.kind === 'runtime')!
  assert.equal(rt.lane, 'runtime')
  assert.ok(rt.failed)
})

test('row packing separates overlapping items within a lane', () => {
  const items = buildItems([
    ev(1, 'agent.tool.started', { timestamp: at(0), operation_id: 'tool:a' }),
    ev(2, 'agent.tool.completed', { timestamp: at(10), operation_id: 'tool:a' }),
    ev(3, 'agent.tool.started', { timestamp: at(2), operation_id: 'tool:b' }),
    ev(4, 'agent.tool.completed', { timestamp: at(4), operation_id: 'tool:b' }),
    ev(5, 'file.read', { timestamp: at(12), path: '/a/b.ts' }),
  ])
  const rows = packRows(items, 0)
  assert.equal(rows.get('claude-code'), 2)
  const a = items.find(i => i.seq === 2)!
  const b = items.find(i => i.seq === 4)!
  assert.notEqual(a.row, b.row)
  assert.equal(items.find(i => i.seq === 5)!.row, 0)
})

test('session bars span observed activity and skip sessions outside the window', () => {
  const s = (id: string, a: number, b: number) => ({
    session_id: id, agent_name: 'codex', first_event_at: at(a), last_event_at: at(b), status: 'ended', project_id: '/p',
  }) as unknown as SessionView
  const bars = sessionBars([s('x', 0, 10), s('y', 5, 20), s('z', 100, 110)], Date.parse(at(0)), Date.parse(at(50)))
  assert.deepEqual(bars.map(b => [b.sessionId, b.row]), [['x', 0], ['y', 1]])
})

test('zoom keeps the anchor in place and respects bounds', () => {
  const [a, b] = zoom(0, 100_000, 0.5, 50_000)
  assert.deepEqual([a, b], [25_000, 75_000])
  const [c, d] = zoom(0, 10_000, 0.01, 0)
  assert.equal(d - c, 5000, 'minimum span')
  assert.equal(tickStep(60 * 60e3), 10 * 60e3)
})

// ── router ──────────────────────────────────────────────────────────────────

test('hash routes round-trip, including session ids with colons', () => {
  const h = href('session', { tab: 'events' }, 'claude-code:abc-1')
  assert.equal(h, '#/sessions/claude-code%3Aabc-1?tab=events')
  const r = parseHash(h)
  assert.equal(r.page, 'session')
  assert.equal(r.id, 'claude-code:abc-1')
  assert.equal(r.params.get('tab'), 'events')
  assert.equal(parseHash('#/nope').page, 'overview')
  assert.equal(parseHash('').page, 'overview')
  assert.equal(href('activity', { q: 'a b', empty: '', agent: 'codex' }), '#/activity?agent=codex&q=a+b')
  assert.deepEqual(withParams(new URLSearchParams('a=1&b=2'), { a: null, c: 3 }), { b: '2', c: '3' })
})

// ── format ──────────────────────────────────────────────────────────────────

test('time formatting in UTC and durations', () => {
  assert.equal(clock('2026-11-25T03:04:05.006Z', 'utc', true), '03:04:05.006')
  assert.equal(dateTime('2026-11-25T03:04:05Z', 'utc'), '2026-11-25 03:04:05 UTC')
  assert.equal(clock(null, 'utc'), '—')
  assert.equal(clock('garbage', 'utc'), '—')
  assert.equal(duration(850), '850 ms')
  assert.equal(duration(4200), '4.2 s')
  assert.equal(duration(192_000), '3 m 12 s')
  assert.equal(duration(3_900_000), '1 h 05 m')
  assert.equal(duration(undefined), '—')
  const now = Date.parse('2026-11-25T10:00:00Z')
  assert.equal(relative('2026-11-25T09:57:00Z', now), '3 min ago')
  assert.equal(relative('2026-11-25T10:05:00Z', now), 'in the future')
  assert.equal(shortPath('/a/b/c/d.ts'), '…/c/d.ts')
  assert.equal(relativeTo('/work/app/src/x.ts', '/work/app'), 'src/x.ts')
  assert.equal(relativeTo('/other/x.ts', '/work/app'), '/other/x.ts')
  assert.equal(compact(12_345), '12.3k')
})

test('settings from storage are validated field by field', () => {
  assert.deepEqual(sanitize(null), DEFAULT_SETTINGS)
  assert.deepEqual(sanitize({ theme: 'light', tz: 'mars', wrap: 'yes', density: 'compact' }),
    { ...DEFAULT_SETTINGS, theme: 'light', density: 'compact' })
})

// ── event wording ───────────────────────────────────────────────────────────

test('file events are worded as reported, never as verified', () => {
  const s = statusOf(ev(1, 'file.modified', { path: '/w/a.ts' }))
  assert.equal(s.label, 'Reported modified')
  assert.match(s.detail, /not independently verified/)
  assert.equal(summary(ev(1, 'file.read', { path: '/w/a.ts' })), 'Read /w/a.ts')
})

test('a missing outcome is never shown as success', () => {
  const codex = ev(1, 'agent.tool.completed', {
    agent_name: 'codex', attributes: { 'tool.name': 'Bash', 'tool.outcome': 'not_reported', 'command.outcome': 'unknown' },
  })
  assert.equal(statusOf(codex).label, 'Outcome not reported')
  assert.equal(summary(codex), 'Bash returned')
  assert.equal(statusOf(ev(2, 'agent.tool.completed', { status: 'succeeded' })).label, 'Succeeded')
  const failed = ev(3, 'command.failed', { status: 'failed', attributes: { 'command.program': 'cargo', 'command.exit_code': 101 } })
  assert.equal(summary(failed), 'cargo exited 101')
  assert.ok(isFailure(failed))
  assert.equal(summary(ev(4, 'command.failed', { status: 'failed', attributes: { 'command.program': 'npm' } })), 'npm failed')
  assert.equal(agentLabel('codex'), 'Codex')
  assert.equal(agentLabel(undefined), 'Runtime')
})
