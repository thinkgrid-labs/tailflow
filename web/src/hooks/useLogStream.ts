import { useState, useEffect, useCallback, useRef } from 'preact/hooks'
import type { LogRecord, ConnectionStatus } from '../types'

const MAX_RECORDS = 5_000

export function useLogStream() {
  const [records, setRecords] = useState<LogRecord[]>([])
  const [status, setStatus] = useState<ConnectionStatus>('connecting')

  // Batch incoming records to requestAnimationFrame cadence (~60fps max)
  // so a high-velocity log stream doesn't thrash the render loop.
  const pending = useRef<LogRecord[]>([])
  const raf     = useRef<number | null>(null)

  const flush = useCallback(() => {
    raf.current = null
    const batch = pending.current.splice(0)
    if (batch.length === 0) return
    setRecords(prev => {
      const bySeq = new Map<number, LogRecord>()
      for (const record of prev) bySeq.set(record.seq, record)
      for (const record of batch) bySeq.set(record.seq, record)
      const next = [...bySeq.values()].sort((a, b) => a.seq - b.seq)
      return next.length > MAX_RECORDS
        ? next.slice(next.length - MAX_RECORDS)
        : next
    })
  }, [])

  useEffect(() => {
    let active = true
    let hydrating = false
    const es = new EventSource('/events')

    const hydrate = () => {
      if (!active || hydrating) return
      hydrating = true
      fetch('/api/records')
        .then(response => response.ok
          ? response.json()
          : Promise.reject(new Error('history request failed')))
        .then((history: LogRecord[]) => {
          if (!active) return
          pending.current.push(...history)
          if (raf.current === null) raf.current = requestAnimationFrame(flush)
        })
        .catch(() => { /* the live stream remains useful without history */ })
        .finally(() => { hydrating = false })
    }

    es.onopen = () => setStatus('connected')

    es.onmessage = (e: MessageEvent) => {
      try {
        pending.current.push(JSON.parse(e.data) as LogRecord)
        if (raf.current === null) {
          raf.current = requestAnimationFrame(flush)
        }
      } catch {
        // ignore malformed events
      }
    }

    es.onerror = () => {
      // EventSource reconnects automatically after transient daemon restarts.
      setStatus(es.readyState === EventSource.CLOSED ? 'disconnected' : 'connecting')
      // Rehydrate on reconnect errors so anything emitted while SSE was down
      // is recovered from the daemon's bounded buffer.
      hydrate()
    }

    // Subscribe first, then hydrate retained history. Sequence IDs deduplicate
    // records that arrive through both paths during the race window.
    hydrate()

    return () => {
      active = false
      es.close()
      if (raf.current !== null) cancelAnimationFrame(raf.current)
    }
  }, [flush])

  const clear = useCallback(() => {
    pending.current = []
    setRecords([])
  }, [])

  return { records, status, clear }
}
