// Hash routing: `#/sessions?agent=codex&status=active`. The whole UI state
// that matters for an investigation lives in the URL, so any view can be
// copied, bookmarked or reloaded. No server change is needed: everything
// after `#` stays in the browser.

export type Page =
  | 'overview'
  | 'sessions'
  | 'session'
  | 'activity'
  | 'timeline'
  | 'logs'
  | 'errors'
  | 'files'
  | 'commands'
  | 'integrations'
  | 'settings'

export interface Route {
  page: Page
  /** Path parameter, e.g. the session id for `session`. */
  id?: string
  params: URLSearchParams
}

const PAGES: readonly Page[] = [
  'overview', 'sessions', 'activity', 'timeline', 'logs', 'errors',
  'files', 'commands', 'integrations', 'settings',
]

/** Parse `location.hash`. Unknown routes fall back to the overview. */
export function parseHash(hash: string): Route {
  const raw = hash.replace(/^#\/?/, '')
  const q = raw.indexOf('?')
  const path = q === -1 ? raw : raw.slice(0, q)
  const params = new URLSearchParams(q === -1 ? '' : raw.slice(q + 1))
  const [head, ...rest] = path.split('/')
  if (head === 'sessions' && rest.length > 0 && rest.join('/') !== '') {
    let id = rest.join('/')
    try { id = decodeURIComponent(id) } catch { /* keep raw */ }
    return { page: 'session', id, params }
  }
  const page = (PAGES as readonly string[]).includes(head) ? (head as Page) : 'overview'
  return { page, params }
}

/** Build a hash for `page` with non-empty `params`, keys sorted for stable URLs. */
export function href(
  page: Page,
  params: Record<string, string | number | boolean | null | undefined> = {},
  id?: string,
): string {
  const path = page === 'session' ? `sessions/${encodeURIComponent(id ?? '')}` : page
  const sp = new URLSearchParams()
  for (const key of Object.keys(params).sort()) {
    const v = params[key]
    if (v === null || v === undefined || v === '' || v === false) continue
    sp.set(key, String(v))
  }
  const qs = sp.toString()
  return `#/${path}${qs ? `?${qs}` : ''}`
}

/** Params as a plain record (first value per key). */
export function toRecord(params: URLSearchParams): Record<string, string> {
  const out: Record<string, string> = {}
  params.forEach((v, k) => { if (!(k in out)) out[k] = v })
  return out
}

/** A copy of `params` with `changes` applied (`null`/`''` removes a key). */
export function withParams(
  params: URLSearchParams,
  changes: Record<string, string | number | null | undefined>,
): Record<string, string> {
  const next = toRecord(params)
  for (const [k, v] of Object.entries(changes)) {
    if (v === null || v === undefined || v === '') delete next[k]
    else next[k] = String(v)
  }
  return next
}
