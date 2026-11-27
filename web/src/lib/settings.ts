// Per-viewer display preferences. Stored in localStorage when available;
// the dashboard works identically without it (private windows, blocked
// storage). Nothing here is sent anywhere.

import type { TimeZoneMode } from './format.ts'

export type ThemeMode = 'system' | 'dark' | 'light'
export type Density = 'comfortable' | 'compact'

export interface Settings {
  theme: ThemeMode
  tz: TimeZoneMode
  density: Density
  /** Wrap long log lines instead of scrolling horizontally. */
  wrap: boolean
  sidebarCollapsed: boolean
}

export const DEFAULT_SETTINGS: Settings = {
  theme: 'system',
  tz: 'local',
  density: 'comfortable',
  wrap: false,
  sidebarCollapsed: false,
}

const KEY = 'tailflow.settings.v1'

/** Validate stored JSON field by field; anything unexpected falls back. */
export function sanitize(raw: unknown): Settings {
  const s = { ...DEFAULT_SETTINGS }
  if (!raw || typeof raw !== 'object') return s
  const r = raw as Record<string, unknown>
  if (r.theme === 'system' || r.theme === 'dark' || r.theme === 'light') s.theme = r.theme
  if (r.tz === 'local' || r.tz === 'utc') s.tz = r.tz
  if (r.density === 'comfortable' || r.density === 'compact') s.density = r.density
  if (typeof r.wrap === 'boolean') s.wrap = r.wrap
  if (typeof r.sidebarCollapsed === 'boolean') s.sidebarCollapsed = r.sidebarCollapsed
  return s
}

export function loadSettings(): Settings {
  try {
    const text = globalThis.localStorage?.getItem(KEY)
    return sanitize(text ? JSON.parse(text) : null)
  } catch {
    return { ...DEFAULT_SETTINGS }
  }
}

export function saveSettings(s: Settings): void {
  try {
    globalThis.localStorage?.setItem(KEY, JSON.stringify(s))
  } catch { /* storage unavailable: the setting lasts for this tab */ }
}

/** Apply theme and density to the document root. */
export function applySettings(s: Settings, doc: Document = document): void {
  const root = doc.documentElement
  if (s.theme === 'system') root.removeAttribute('data-theme')
  else root.setAttribute('data-theme', s.theme)
  root.setAttribute('data-density', s.density)
}
