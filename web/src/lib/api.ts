// Typed, abortable access to the daemon's read-only API. Same origin only:
// the dashboard never talks to anything but the daemon that served it.

import type {
  AgentsStatus, Collector, EventPage, FilesPage, Health, SessionDetail,
  SessionPage, Stats, StorageStatus, StoredEvent,
} from './types.ts'

export class ApiError extends Error {
  status: number
  kind: string
  constructor(status: number, kind: string, message: string) {
    super(message)
    this.status = status
    this.kind = kind
  }
}

export type Params = Record<string, string | number | boolean | null | undefined>

export function query(params: Params): string {
  const sp = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) {
    if (v === null || v === undefined || v === '') continue
    sp.set(k, String(v))
  }
  const s = sp.toString()
  return s ? `?${s}` : ''
}

export async function getJSON<T>(path: string, params: Params = {}, signal?: AbortSignal): Promise<T> {
  let res: Response
  try {
    res = await fetch(`${path}${query(params)}`, { signal, headers: { Accept: 'application/json' } })
  } catch (e) {
    if ((e as Error).name === 'AbortError') throw e
    throw new ApiError(0, 'unreachable', 'The TailFlow daemon is not reachable. Is it still running?')
  }
  if (!res.ok) {
    let kind = 'http_error'
    let message = `Request failed (${res.status})`
    try {
      const body = await res.json() as { error?: unknown; kind?: unknown }
      if (typeof body.error === 'string') message = body.error
      if (typeof body.kind === 'string') kind = body.kind
    } catch { /* non-JSON error body */ }
    throw new ApiError(res.status, kind, message)
  }
  return res.json() as Promise<T>
}

export const api = {
  health: (s?: AbortSignal) => getJSON<Health>('/health', {}, s),
  stats: (p: Params, s?: AbortSignal) => getJSON<Stats>('/api/v1/stats', p, s),
  events: (p: Params, s?: AbortSignal) => getJSON<EventPage>('/api/v1/events', p, s),
  event: (seq: number, s?: AbortSignal) => getJSON<StoredEvent>(`/api/v1/events/${seq}`, { include_payload: true }, s),
  sessions: (p: Params, s?: AbortSignal) => getJSON<SessionPage>('/api/v1/sessions', p, s),
  session: (id: string, s?: AbortSignal) =>
    getJSON<SessionDetail>(`/api/v1/sessions/${encodeURIComponent(id)}`, {}, s),
  files: (p: Params, s?: AbortSignal) => getJSON<FilesPage>('/api/v1/files', p, s),
  agents: (s?: AbortSignal) => getJSON<AgentsStatus>('/api/v1/agents/status', {}, s),
  storage: (s?: AbortSignal) => getJSON<StorageStatus>('/api/v1/storage/status', {}, s),
  collectors: (s?: AbortSignal) =>
    getJSON<{ collectors?: Collector[] }>('/api/v1/collectors/status', {}, s),
  privacy: (s?: AbortSignal) =>
    getJSON<{ capture_mode: string; capture_enabled: boolean; redaction_active: boolean; redaction_rules: string[]; guarantee: string }>('/api/v1/privacy', {}, s),
}

/** Short explanation for an error shown in the UI. */
export function describeError(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.kind === 'storage_disabled') return 'Durable history is disabled on this daemon (--no-storage or [storage] enabled = false). Live logs still work.'
    if (e.kind === 'storage_unavailable') return 'Durable history is unavailable. See Integrations → Storage for the reason.'
    if (e.kind === 'query_timeout') return 'The query hit the 3-second budget. Narrow the time range or add a filter.'
    return e.message
  }
  return 'Unexpected error.'
}
