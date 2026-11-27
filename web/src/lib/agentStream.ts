// Live agent activity (M4-B): one EventSource to /api/v1/stream/agent,
// shared by every page.
//
// The daemon sends only events already committed to durable history, in
// seq order, each with its seq as the SSE `id`. EventSource resends the last
// id when it reconnects (network loss, daemon restart), so the daemon
// catches us up from storage without duplicates. Pages do not consume the
// events directly: they register a predicate, and a matching event triggers
// a coalesced reload of that page's own bounded query. That keeps every view
// consistent with the REST API, which stays the single source of what a
// page shows, while replacing periodic polling as the update mechanism.

import { checkSession } from './auth.ts'

export type AgentStreamStatus = 'off' | 'connecting' | 'live' | 'reconnecting'

/** The fields pages filter on. */
export interface StreamEvent {
  seq: number
  event_type: string
  agent_name?: string | null
  session_id?: string | null
  project_id?: string | null
  path?: string | null
  status?: string | null
}

export type Signal =
  | { kind: 'event'; event: StreamEvent }
  /** History may have changed in ways events alone do not describe:
   *  a gap, a reset after a long disconnect, or a fresh (re)connection. */
  | { kind: 'resync'; reason: 'gap' | 'reset' | 'connected' }

type Listener = (s: Signal) => void

/** Close the stream after the tab has been hidden this long; reopen (with
 *  catch-up from the last seq) when it is shown again. */
const HIDDEN_CLOSE_MS = 60_000

class AgentStream {
  status: AgentStreamStatus = 'off'
  /** Last delivered seq (also the resume cursor). */
  lastSeq: number | null = null
  /** Loss reported by the daemon since this page loaded. */
  gaps = 0
  /** Times the stream had to resynchronize (lag, reset, reconnect). */
  resyncs = 0
  private es: EventSource | null = null
  private listeners = new Set<Listener>()
  private statusListeners = new Set<() => void>()
  private hiddenTimer: ReturnType<typeof setTimeout> | null = null
  private wired = false

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn)
    this.ensure()
    return () => {
      this.listeners.delete(fn)
    }
  }

  onStatus(fn: () => void): () => void {
    this.statusListeners.add(fn)
    this.ensure()
    return () => { this.statusListeners.delete(fn) }
  }

  private setStatus(s: AgentStreamStatus): void {
    if (this.status === s) return
    this.status = s
    for (const l of this.statusListeners) l()
  }

  private emit(s: Signal): void {
    for (const l of this.listeners) l(s)
  }

  private ensure(): void {
    if (!this.wired) {
      this.wired = true
      document.addEventListener('visibilitychange', () => this.onVisibility())
    }
    if (!this.es && document.visibilityState !== 'hidden') this.open()
  }

  private onVisibility(): void {
    if (document.visibilityState === 'hidden') {
      if (this.hiddenTimer === null) {
        this.hiddenTimer = setTimeout(() => {
          this.hiddenTimer = null
          this.close()
        }, HIDDEN_CLOSE_MS)
      }
    } else {
      if (this.hiddenTimer !== null) { clearTimeout(this.hiddenTimer); this.hiddenTimer = null }
      if (!this.es && (this.listeners.size > 0 || this.statusListeners.size > 0)) this.open()
    }
  }

  private open(): void {
    const url = this.lastSeq === null ? '/api/v1/stream/agent' : `/api/v1/stream/agent?after=${this.lastSeq}`
    const es = new EventSource(url)
    this.es = es
    this.setStatus(this.lastSeq === null ? 'connecting' : 'reconnecting')
    es.addEventListener('agent', (e: MessageEvent) => {
      try {
        const ev = JSON.parse(e.data) as StreamEvent
        if (typeof ev.seq !== 'number') return
        if (this.lastSeq !== null && ev.seq <= this.lastSeq) return // never twice
        this.lastSeq = ev.seq
        this.emit({ kind: 'event', event: ev })
      } catch { /* malformed: ignore */ }
    })
    es.addEventListener('ready', (e: MessageEvent) => {
      try {
        const { cursor } = JSON.parse(e.data) as { cursor?: number }
        if (typeof cursor === 'number') this.lastSeq = Math.max(this.lastSeq ?? 0, cursor)
      } catch { /* keep the cursor we have */ }
      this.setStatus('live')
      this.resyncs++
      this.emit({ kind: 'resync', reason: 'connected' })
    })
    es.addEventListener('gap', () => {
      this.gaps++
      this.emit({ kind: 'resync', reason: 'gap' })
    })
    es.addEventListener('reset', (e: MessageEvent) => {
      try {
        const { cursor } = JSON.parse(e.data) as { cursor?: number }
        if (typeof cursor === 'number') this.lastSeq = cursor
      } catch { /* ignore */ }
      this.resyncs++
      this.emit({ kind: 'resync', reason: 'reset' })
    })
    es.addEventListener('lag', () => { this.resyncs++ })
    es.onerror = () => {
      if (es.readyState === EventSource.CLOSED) {
        // Refused (auth, storage off, too many streams) or the server
        // ended the stream: stop; pages fall back to polling.
        this.es = null
        this.setStatus('off')
        void checkSession()
      } else {
        this.setStatus('reconnecting')
      }
    }
  }

  private close(): void {
    this.es?.close()
    this.es = null
    this.setStatus('off')
  }
}

export const agentStream = new AgentStream()
