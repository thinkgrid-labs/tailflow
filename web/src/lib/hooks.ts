// Shared Preact hooks: routing, settings, abortable loads and
// visibility-aware polling. Hidden tabs do no polling and no rendering work.

import { createContext } from 'preact'
import { useCallback, useContext, useEffect, useRef, useState } from 'preact/hooks'
import { parseHash, type Route } from './router.ts'
import { DEFAULT_SETTINGS, type Settings } from './settings.ts'

export function useRoute(): Route {
  const [route, setRoute] = useState(() => parseHash(location.hash))
  useEffect(() => {
    const on = () => setRoute(parseHash(location.hash))
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

export function navigate(hash: string, replace = false): void {
  if (replace) history.replaceState(null, '', hash)
  else location.hash = hash
  if (replace) window.dispatchEvent(new HashChangeEvent('hashchange'))
}

export const SettingsContext = createContext<{ settings: Settings; update: (p: Partial<Settings>) => void }>({
  settings: DEFAULT_SETTINGS,
  update: () => {},
})

export function useSettings() {
  return useContext(SettingsContext)
}

export function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => document.visibilityState !== 'hidden')
  useEffect(() => {
    const on = () => setVisible(document.visibilityState !== 'hidden')
    document.addEventListener('visibilitychange', on)
    return () => document.removeEventListener('visibilitychange', on)
  }, [])
  return visible
}

export interface AsyncState<T> {
  data: T | null
  error: unknown
  loading: boolean
  /** Re-run now (keeps showing the current data while loading). */
  reload: () => void
}

/**
 * Run `load` when `deps` change, aborting the previous request. With
 * `pollMs`, re-run on that interval while the page is visible, and once
 * immediately when it becomes visible again.
 */
export function useAsync<T>(
  load: (signal: AbortSignal) => Promise<T>,
  deps: readonly unknown[],
  pollMs?: number,
): AsyncState<T> {
  // Results are keyed by their deps, so data loaded for a previous filter is
  // never shown under the new one, even for a frame.
  const key = JSON.stringify(deps)
  const [state, setState] = useState<{ key: string; data: T | null; error: unknown; loading: boolean }>(
    { key, data: null, error: null, loading: true })
  const [tick, setTick] = useState(0)
  const visible = usePageVisible()
  const loadRef = useRef(load)
  loadRef.current = load

  useEffect(() => {
    if (!visible && tick > 0) return
    const ctrl = new AbortController()
    setState(s => (s.key === key ? { ...s, loading: true } : { key, data: null, error: null, loading: true }))
    loadRef.current(ctrl.signal)
      .then(data => { if (!ctrl.signal.aborted) setState({ key, data, error: null, loading: false }) })
      .catch(error => {
        if (ctrl.signal.aborted || (error as Error)?.name === 'AbortError') return
        setState(s => ({ key, data: s.key === key ? s.data : null, error, loading: false }))
      })
    return () => ctrl.abort()
  }, [key, tick, visible])

  useEffect(() => {
    if (!pollMs || !visible) return
    const t = setInterval(() => setTick(n => n + 1), pollMs)
    return () => clearInterval(t)
  }, [pollMs, visible])

  const reload = useCallback(() => setTick(n => n + 1), [])
  const current = state.key === key
  return {
    data: current ? state.data : null,
    error: current ? state.error : null,
    loading: current ? state.loading : true,
    reload,
  }
}

export function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value)
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms)
    return () => clearTimeout(t)
  }, [value, ms])
  return v
}

/** Re-render every `ms` while visible (for "3 min ago" labels). */
export function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now())
  const visible = usePageVisible()
  useEffect(() => {
    if (!visible) return
    setNow(Date.now())
    const t = setInterval(() => setNow(Date.now()), ms)
    return () => clearInterval(t)
  }, [ms, visible])
  return now
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    // Clipboard API refused (permissions, insecure context): fall back.
    const ta = document.createElement('textarea')
    ta.value = text
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.select()
    let ok = false
    try { ok = document.execCommand('copy') } catch { ok = false }
    ta.remove()
    return ok
  }
}
