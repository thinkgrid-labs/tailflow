// Browser access to the daemon (M4-A).
//
// The dashboard never holds a credential in script: the daemon sets an
// HttpOnly session cookie when the user opens a one-time sign-in link, and
// the browser attaches it to same-origin requests. All this module tracks
// is whether that session currently works, so the app can show the
// sign-in screen instead of a page full of errors.

export type AuthStatus = 'checking' | 'signed_in' | 'signed_out'

/** Error kinds the daemon uses for refused credentials. */
const REFUSED = new Set(['unauthorized', 'invalid_token', 'session_expired', 'cross_origin'])

export function isRefusal(status: number, kind: string | undefined): boolean {
  return (status === 401 || status === 403) && kind !== undefined && REFUSED.has(kind)
}

export interface AuthState {
  status: AuthStatus
  /** `kind` of the refusal that signed us out, when known. */
  reason: string | null
}

let state: AuthState = { status: 'checking', reason: null }
const listeners = new Set<() => void>()

export function authState(): AuthState {
  return state
}

function set(next: AuthState): void {
  if (next.status === state.status && next.reason === state.reason) return
  state = next
  for (const l of listeners) l()
}

export function subscribeAuth(fn: () => void): () => void {
  listeners.add(fn)
  return () => { listeners.delete(fn) }
}

/** A request was refused for lack of a valid session. */
export function markSignedOut(reason: string | null): void {
  set({ status: 'signed_out', reason })
}

let checking: Promise<AuthStatus> | null = null

/** Ask the daemon whether this browser's session is valid. Network
 *  failures leave the state unchanged: "daemon down" is not "signed out". */
export function checkSession(): Promise<AuthStatus> {
  if (checking) return checking
  checking = (async () => {
    try {
      const r = await fetch('/api/v1/auth/session', { headers: { Accept: 'application/json' }, signal: AbortSignal.timeout(10_000) })
      if (r.ok) {
        set({ status: 'signed_in', reason: null })
      } else {
        let kind: string | undefined
        try { kind = ((await r.json()) as { kind?: unknown }).kind as string | undefined } catch { /* non-JSON */ }
        if (isRefusal(r.status, kind)) set({ status: 'signed_out', reason: kind ?? null })
      }
    } catch { /* unreachable: keep the current state */ }
    return state.status
  })().finally(() => { checking = null })
  return checking
}

/** The command that prints or opens a fresh sign-in link for this daemon. */
export function signInCommand(port: string): string {
  return port && port !== '7878' ? `tailflow-daemon dashboard --port ${port}` : 'tailflow-daemon dashboard'
}

export function signOut(): Promise<void> {
  return fetch('/api/v1/auth/logout', { method: 'POST' })
    .then(() => markSignedOut('signed_out'))
    .catch(() => markSignedOut('signed_out'))
}
