import type { LogRecord } from '../types'
import { sourceColor, LEVEL_COLOR } from '../types'

interface Props {
  record: LogRecord
}

function formatTs(iso: string): string {
  // Show only HH:MM:SS.mmm from the ISO timestamp
  const d = new Date(iso)
  const hh = String(d.getHours()).padStart(2, '0')
  const mm = String(d.getMinutes()).padStart(2, '0')
  const ss = String(d.getSeconds()).padStart(2, '0')
  const ms = String(d.getMilliseconds()).padStart(3, '0')
  return `${hh}:${mm}:${ss}.${ms}`
}

export function LogRow({ record }: Props) {
  const sc = sourceColor(record.source)
  const lc = LEVEL_COLOR[record.level]

  return (
    <div class="log-row">
      <span class="log-ts">{formatTs(record.timestamp)}</span>
      <span class="log-source" style={{ color: sc }}>
        {record.source.slice(0, 16).padEnd(16, ' ')}
      </span>
      <span class="log-level" style={{ color: lc }}>
        {record.level.slice(0, 5).toUpperCase().padEnd(5, ' ')}
      </span>
      <span class="log-payload">{record.payload}</span>
    </div>
  )
}
