import { useEffect, useState } from 'preact/hooks'

// Read-only view of durable storage and ingestion health. Polls only while
// open, so a closed panel costs nothing.

interface StorageStatus {
  enabled: boolean
  state: string
  kind?: string
  error?: string
  path?: string
  capture_mode?: string
  capture_enabled?: boolean
  full_text_search?: boolean
  recovered_unclean_runs?: number
  db?: {
    events: number
    oldest_event_at: string | null
    newest_event_at: string | null
    file_bytes: number
    wal_bytes: number
    used_bytes: number
    gaps: number
    evicted_through_seq: number
  }
  retention?: {
    retention_days: number | null
    payload_retention_days: number
    max_size_mb: number
    last_cleanup_at: string | null
  }
  health?: {
    events_persisted: number
    events_dropped_total: number
    events_dropped: Record<string, number>
    events_excluded: number
    storage_errors: number
    ingestion_queue_depth: number
    loss_detected: boolean
    pending_loss_unrecorded: number
    last_error: string | null
    last_commit_at: string | null
  }
  ingest?: {
    records_received: number
    records_dropped: number
    records_redacted: number
    sse_client_lag_records: number
  }
}

interface Collector {
  name: string
  kind: string
  collector_status: string
  events_received: number
  last_event_at: string | null
  detail?: string
}

function mb(bytes: number | undefined): string {
  return bytes === undefined ? '—' : `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

function when(ts: string | null | undefined): string {
  return ts ? new Date(ts).toLocaleString() : '—'
}

function Row({ label, value, warn }: { label: string; value: string | number; warn?: boolean }) {
  return (
    <div class={`diag-row${warn ? ' diag-row--warn' : ''}`}>
      <span class="diag-label">{label}</span>
      <span class="diag-value">{value}</span>
    </div>
  )
}

export function Diagnostics({ onClose }: { onClose: () => void }) {
  const [storage, setStorage] = useState<StorageStatus | null>(null)
  const [collectors, setCollectors] = useState<Collector[]>([])
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    let active = true
    const load = () => {
      Promise.all([
        fetch('/api/v1/storage/status').then(r => r.json()),
        fetch('/api/v1/collectors/status').then(r => r.json()),
      ])
        .then(([s, c]) => {
          if (!active) return
          setStorage(s as StorageStatus)
          setCollectors((c as { collectors?: Collector[] }).collectors ?? [])
          setFailed(false)
        })
        .catch(() => { if (active) setFailed(true) })
    }
    load()
    const timer = setInterval(load, 5000)
    return () => { active = false; clearInterval(timer) }
  }, [])

  const h = storage?.health
  const db = storage?.db
  const loss = (storage?.ingest?.records_dropped ?? 0) + (h?.events_dropped_total ?? 0)

  return (
    <aside class="diag" aria-label="Diagnostics">
      <div class="diag-head">
        <span>Diagnostics</span>
        <button class="clear-btn" onClick={onClose} title="Close diagnostics">✕</button>
      </div>
      {failed && <p class="diag-note diag-row--warn">Daemon unreachable or too old for /api/v1.</p>}
      {storage && (
        <>
          <h3 class="diag-section">Storage</h3>
          <Row label="state" value={storage.enabled ? storage.state : 'disabled'}
               warn={storage.state !== 'ok' && storage.enabled} />
          {storage.error && <p class="diag-note diag-row--warn">{storage.error}</p>}
          {storage.path && <Row label="database" value={storage.path} />}
          <Row label="size" value={db ? `${mb(db.file_bytes + db.wal_bytes)} (${mb(db.used_bytes)} live)` : '—'} />
          <Row label="events" value={db?.events.toLocaleString() ?? '—'} />
          <Row label="oldest retained" value={when(db?.oldest_event_at)} />
          <Row label="retention"
               value={storage.retention
                 ? `${storage.retention.retention_days ?? 'unlimited'} d · payloads ${storage.retention.payload_retention_days} d · cap ${storage.retention.max_size_mb} MB`
                 : '—'} />
          <Row label="last cleanup" value={when(storage.retention?.last_cleanup_at)} />
          <Row label="full-text search" value={storage.full_text_search === undefined ? '—' : storage.full_text_search ? 'on' : 'off'} />

          <h3 class="diag-section">Completeness</h3>
          <Row label="loss detected" value={loss > 0 || h?.loss_detected ? 'YES — history is incomplete' : 'no'}
               warn={loss > 0 || !!h?.loss_detected} />
          <Row label="dropped at ingestion" value={storage.ingest?.records_dropped ?? 0}
               warn={(storage.ingest?.records_dropped ?? 0) > 0} />
          <Row label="dropped before disk" value={h?.events_dropped_total ?? '—'}
               warn={(h?.events_dropped_total ?? 0) > 0} />
          <Row label="recorded gaps" value={db?.gaps ?? '—'} warn={(db?.gaps ?? 0) > 0} />
          <Row label="unclean previous runs" value={storage.recovered_unclean_runs ?? 0}
               warn={(storage.recovered_unclean_runs ?? 0) > 0} />
          <Row label="persistence errors" value={h?.storage_errors ?? '—'} warn={(h?.storage_errors ?? 0) > 0} />
          {h?.last_error && <p class="diag-note diag-row--warn">{h.last_error}</p>}
          <Row label="persisted (this run)" value={h?.events_persisted.toLocaleString() ?? '—'} />
          <Row label="queue depth" value={h?.ingestion_queue_depth ?? '—'} />

          <h3 class="diag-section">Privacy</h3>
          <Row label="capture mode" value={storage.capture_mode ?? '—'} />
          <Row label="capture enabled" value={storage.capture_enabled === false ? 'no' : 'yes'} />
          <Row label="lines redacted" value={storage.ingest?.records_redacted ?? 0} />
          <Row label="excluded by policy" value={h?.events_excluded ?? 0} />
        </>
      )}
      <h3 class="diag-section">Collectors</h3>
      {collectors.length === 0 && <p class="diag-note">No collectors reported.</p>}
      {collectors.map(c => (
        <Row key={c.name}
             label={`${c.name} (${c.kind})`}
             value={`${c.collector_status} · ${c.events_received.toLocaleString()} events`}
             warn={c.collector_status === 'failed'} />
      ))}
    </aside>
  )
}
