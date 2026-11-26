import { useEffect, useState } from 'preact/hooks'
import type { ConnectionStatus, Health, LogRecord } from '../lib/types.ts'
import { append, boundPending, lastSeq, MAX_RECORDS } from '../lib/logStore.ts'
import { checkSession } from '../lib/auth.ts'

/**
 * Live runtime log stream, shared by the whole app (one EventSource).
 *
 * - SSE records are batched to animation frames, and only applied while a
 *   view that shows them is mounted and not frozen. Otherwise they wait in
 *   a bounded queue (newest kept), so other pages and hidden tabs pay
 *   nothing per frame and memory stays bounded.
 * - On (re)connect the view catches up once from the daemon ring with an
 *   `after` cursor instead of re-reading the whole ring on every error.
 * - The ring's seq restarts with each daemon process: a changed
 *   `instance_id` resets the view instead of mixing two processes.
 * - Frozen (the user scrolled up or paused): records queue instead of
 *   shifting the rows being read; applied on resume.
 */
/** Minimum interval between applied batches. */
const FLUSH_MS = 100

class LiveStream {
  records: LogRecord[] = []
  status: ConnectionStatus = 'connecting'
  /** Lines the daemon dropped for this subscriber (SSE `gap` events). */
  missed = 0
  /** Lines that left the daemon's ring before a catch-up could fetch them. */
  lostOnCatchUp = 0
  queued = 0
  droppedWhileFrozen = 0
  restarts = 0

  private pending: LogRecord[] = []
  private frozen = false
  /** Live records wait while a reconnect checks which process is serving:
   *  until then they could belong to a new daemon whose seqs collide. */
  private holding = false
  private raf: number | null = null
  private timer: ReturnType<typeof setTimeout> | null = null
  private lastFlush = 0
  private instance: string | null = null
  private syncing = false
  /** A catch-up requested while one was running: merged, never dropped. */
  private queuedSync: { after: number | null; hold: boolean } | null = null
  private started = false
  /** Seq to resume after on the next sync: captured when the stream broke
   *  (or lagged), before any newer live record can arrive. `null` = load the
   *  whole ring (first connection, or a new daemon process). */
  private resumeAfter: number | null = null
  /** "Clear" hides everything up to this seq; catch-ups never bring it back. */
  private floor: number | null = null
  private hydrated = false
  private viewers = 0
  private listeners = new Set<() => void>()

  start(): void {
    if (this.started) return
    this.started = true
    const es = new EventSource('/events')
    es.onopen = () => {
      this.set({ status: 'connected' })
      if (this.hydrated) {
        this.holding = true
        void this.sync(this.resumeAfter, true)
      } else {
        void this.sync(null, false)
      }
    }
    es.onmessage = (e: MessageEvent) => {
      try {
        this.enqueue([JSON.parse(e.data) as LogRecord])
      } catch { /* ignore malformed events */ }
    }
    // Named `gap` event: this subscriber lagged and the daemon dropped
    // records for it. Count them and catch up from the ring.
    es.addEventListener('gap', (e: MessageEvent) => {
      try {
        const { dropped } = JSON.parse(e.data) as { dropped?: number }
        if (typeof dropped === 'number') this.set({ missed: this.missed + dropped })
      } catch { /* malformed gap event: still catch up */ }
      // Records after the gap have not arrived yet: resume from what we hold.
      void this.sync(this.newestHeld(), false)
    })
    es.onerror = () => {
      // EventSource reconnects by itself; `onopen` then catches up once,
      // from the newest record held when the stream broke.
      if (this.status === 'connected') this.resumeAfter = this.newestHeld()
      this.set({ status: es.readyState === EventSource.CLOSED ? 'disconnected' : 'connecting' })
      // A refused credential (daemon restarted, session expired) closes the
      // stream for good; find out whether that is why, so the app can ask
      // the user to sign in again instead of showing "Disconnected".
      if (es.readyState === EventSource.CLOSED) void checkSession()
    }
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'visible') this.schedule()
    })
  }

  private set(p: Partial<Pick<LiveStream, 'status' | 'missed' | 'lostOnCatchUp' | 'queued' | 'droppedWhileFrozen' | 'restarts' | 'records'>>): void {
    Object.assign(this, p)
    for (const l of this.listeners) l()
  }

  private newestHeld(): number | null {
    const a = lastSeq(this.pending)
    const b = lastSeq(this.records)
    return a === null ? b : b === null ? a : Math.max(a, b)
  }

  private async sync(resumeAfter: number | null, hold: boolean): Promise<void> {
    if (this.syncing) {
      // Merge with any queued request: the earliest cursor wins (null = all).
      const q = this.queuedSync
      const after = q === null ? resumeAfter
        : q.after === null || resumeAfter === null ? null : Math.min(q.after, resumeAfter)
      this.queuedSync = { after, hold: hold || (q?.hold ?? false) }
      return
    }
    this.syncing = true
    try {
      const get = <T,>(url: string) =>
        fetch(url, { signal: AbortSignal.timeout(10_000) }).then(r => (r.ok ? (r.json() as Promise<T>) : null))
      const health = await get<Health>('/health')
      const id = health?.instance_id ?? null
      const restarted = this.instance !== null && id !== null && id !== this.instance
      if (restarted) {
        // A new process numbers its ring from scratch: drop the old view.
        // Live records the new process already sent are in its ring, which
        // is fetched in full below (overlap is deduplicated by seq).
        this.pending = []
        this.floor = null
        this.set({ records: [], restarts: this.restarts + 1 })
      }
      // An unknown answer (error, old daemon) never erases what we know.
      if (id !== null) this.instance = id
      let after = restarted ? null : resumeAfter
      if (!restarted && this.floor !== null) after = after === null ? this.floor : Math.max(after, this.floor)
      const url = after === null ? '/api/records' : `/api/records?after=${after}`
      const history = (await get<LogRecord[]>(url)) ?? []
      // The ring is contiguous: a first seq beyond `after + 1` means lines
      // left the ring before we could fetch them (sleep, long disconnect).
      if (after !== null && history.length > 0 && history[0].seq > after + 1) {
        this.set({ lostOnCatchUp: this.lostOnCatchUp + (history[0].seq - after - 1) })
      }
      this.hydrated = true
      const floor = this.floor
      const keep = floor === null ? history : history.filter(r => r.seq > floor)
      if (keep.length > 0) this.enqueue(keep)
    } catch {
      /* the live stream remains useful without history */
    } finally {
      this.syncing = false
      const q = this.queuedSync
      this.queuedSync = null
      if (q) {
        this.holding = q.hold
        void this.sync(q.after, q.hold)
      } else {
        this.holding = false
        this.schedule()
      }
    }
  }

  private enqueue(batch: LogRecord[]): void {
    this.pending.push(...batch)
    const dropped = boundPending(this.pending, MAX_RECORDS)
    if (dropped > 0 && this.frozen) this.droppedWhileFrozen += dropped
    this.schedule()
  }

  /**
   * Apply queued records at most every FLUSH_MS, on an animation frame. A
   * log tail does not need 60 updates/s, and each update repaints the view:
   * batching to 10/s cuts paint work ~6× at the same visible latency.
   */
  private schedule(): void {
    if (this.raf !== null || this.timer !== null || this.viewers === 0) return
    const wait = Math.max(0, this.lastFlush + FLUSH_MS - performance.now())
    this.timer = setTimeout(() => {
      this.timer = null
      this.raf = requestAnimationFrame(() => this.flush())
    }, wait)
  }

  private flush(): void {
    this.raf = null
    this.lastFlush = performance.now()
    if (this.holding) return
    if (this.frozen || this.viewers === 0) {
      if (this.queued !== this.pending.length) this.set({ queued: this.pending.length })
      return
    }
    const batch = this.pending.splice(0)
    if (batch.length === 0) return
    this.set({ records: append(this.records, batch, MAX_RECORDS).records, queued: 0 })
  }

  setFrozen(v: boolean): void {
    if (this.frozen === v) return
    this.frozen = v
    if (!v) {
      this.droppedWhileFrozen = 0
      this.schedule()
    }
  }

  /** Empty the view. Later catch-ups never bring back what was cleared. */
  clear(): void {
    const newest = this.newestHeld()
    if (newest !== null) this.floor = this.floor === null ? newest : Math.max(this.floor, newest)
    this.pending = []
    this.set({ records: [], queued: 0 })
  }

  subscribe(fn: () => void, viewer: boolean): () => void {
    this.listeners.add(fn)
    if (viewer) {
      this.viewers++
      this.schedule()
    }
    return () => {
      this.listeners.delete(fn)
      if (viewer) this.viewers--
    }
  }
}

export const live = new LiveStream()

function useLive(viewer: boolean) {
  const [, setTick] = useState(0)
  useEffect(() => {
    live.start()
    return live.subscribe(() => setTick(n => n + 1), viewer)
  }, [viewer])
  return live
}

/** Connection state only (cheap: no per-record renders). */
export function useLiveStatus() {
  const [status, setStatus] = useState(live.status)
  useEffect(() => {
    live.start()
    return live.subscribe(() => setStatus(live.status), false)
  }, [])
  return status
}

/** Records and stream state, for the Logs view. */
export function useLogStream() {
  return useLive(true)
}
