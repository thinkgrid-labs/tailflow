export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'unknown'

export interface LogRecord {
  timestamp: string
  source: string
  level: LogLevel
  payload: string
}

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

// Matches the TUI color palette (Cyan → Green → Yellow → Magenta → Blue → …)
export const SOURCE_PALETTE = [
  '#22d3ee',
  '#4ade80',
  '#facc15',
  '#e879f9',
  '#60a5fa',
  '#67e8f9',
  '#86efac',
  '#fde047',
  '#f0abfc',
  '#93c5fd',
] as const

export const LEVEL_COLOR: Record<LogLevel, string> = {
  error:   '#f87171',
  warn:    '#fbbf24',
  info:    '#4ade80',
  debug:   '#60a5fa',
  trace:   '#6b7280',
  unknown: '#9ca3af',
}

const colorIndex = new Map<string, number>()
let nextIndex = 0

export function sourceColor(name: string): string {
  if (!colorIndex.has(name)) {
    colorIndex.set(name, nextIndex++ % SOURCE_PALETTE.length)
  }
  return SOURCE_PALETTE[colorIndex.get(name)!]
}
