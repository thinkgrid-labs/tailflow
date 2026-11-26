// The live log buffer, as pure functions over arrays so it can be tested
// without a browser.
//
// Invariants:
// - records are strictly ascending by `seq`, with no duplicates;
// - at most `cap` records are held (oldest dropped first);
// - records from different daemon processes are never mixed: the ring's
//   `seq` restarts in every process, so a new `instance_id` starts a new
//   buffer instead of being merged by seq.

import type { LogRecord } from './types.ts'

/** Records held for the live view (the daemon ring's default size). */
export const MAX_RECORDS = 5_000

/** True when `batch` is strictly ascending and starts after `prev`'s last seq. */
export function appendsInOrder(prev: readonly LogRecord[], batch: readonly LogRecord[]): boolean {
  let last = prev.length > 0 ? prev[prev.length - 1].seq : -Infinity
  for (const r of batch) {
    if (r.seq <= last) return false
    last = r.seq
  }
  return true
}

/** Deduplicate by seq (the later copy wins) and sort ascending. */
export function mergeBySeq(prev: readonly LogRecord[], batch: readonly LogRecord[]): LogRecord[] {
  const bySeq = new Map<number, LogRecord>()
  for (const r of prev) bySeq.set(r.seq, r)
  for (const r of batch) bySeq.set(r.seq, r)
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq)
}

export interface AppendResult {
  records: LogRecord[]
  /** Records removed from the head to respect the cap (for scroll anchoring). */
  trimmed: number
}

/**
 * Add `batch` to `prev`. Live SSE records arrive in order after everything
 * held, so the common path appends without re-sorting; overlap from a
 * catch-up fetch takes the merge path.
 */
export function append(prev: readonly LogRecord[], batch: readonly LogRecord[], cap = MAX_RECORDS): AppendResult {
  if (batch.length === 0) return { records: prev as LogRecord[], trimmed: 0 }
  const next = appendsInOrder(prev, batch) ? prev.concat(batch) : mergeBySeq(prev, batch)
  if (next.length <= cap) {
    // Records removed by the merge would be duplicates, never trims.
    return { records: next, trimmed: 0 }
  }
  const trimmed = next.length - cap
  // Count how many of the trimmed records the caller had already rendered.
  const firstKept = next[trimmed].seq
  let renderedTrim = 0
  for (const r of prev) {
    if (r.seq < firstKept) renderedTrim++
    else break
  }
  return { records: next.slice(trimmed), trimmed: renderedTrim }
}

/**
 * Bound a queue of not-yet-rendered records. Hidden tabs do not run
 * `requestAnimationFrame`, so without this a background tab at 1,000 lines/s
 * would accumulate millions of records before its next frame.
 */
export function boundPending(pending: LogRecord[], cap = MAX_RECORDS): number {
  if (pending.length <= cap) return 0
  const drop = pending.length - cap
  pending.splice(0, drop)
  return drop
}

export function lastSeq(records: readonly LogRecord[]): number | null {
  return records.length > 0 ? records[records.length - 1].seq : null
}

/** Match a record against the in-memory filter (regex when valid, else substring). */
export function compileFilter(text: string): ((r: LogRecord) => boolean) | null {
  const t = text.trim()
  if (!t) return null
  let re: RegExp | null = null
  const slashed = /^\/(.+)\/([a-z]*)$/.exec(t)
  try {
    re = slashed ? new RegExp(slashed[1], slashed[2].includes('i') ? slashed[2] : `${slashed[2]}i`) : null
  } catch {
    re = null
  }
  if (re) {
    const rx = re
    return r => rx.test(r.payload) || rx.test(r.source)
  }
  const lower = t.toLowerCase()
  return r => r.payload.toLowerCase().includes(lower) || r.source.toLowerCase().includes(lower)
}
