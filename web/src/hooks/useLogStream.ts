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
      const next = [...prev, ...batch]
      return next.length > MAX_RECORDS
        ? next.slice(next.length - MAX_RECORDS)
        : next
    })
  }, [])

  useEffect(() => {
    const es = new EventSource('/events')

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
      setStatus('disconnected')
      es.close()
    }

    return () => {
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
