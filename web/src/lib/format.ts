// Formatting: timestamps (local or UTC, chosen in Settings), durations,
// counts and paths. Pure functions; the time zone is passed in.

export type TimeZoneMode = 'local' | 'utc'

const pad = (n: number, w = 2) => String(n).padStart(w, '0')

function parts(d: Date, tz: TimeZoneMode) {
  return tz === 'utc'
    ? { y: d.getUTCFullYear(), mo: d.getUTCMonth() + 1, d: d.getUTCDate(), h: d.getUTCHours(), mi: d.getUTCMinutes(), s: d.getUTCSeconds(), ms: d.getUTCMilliseconds() }
    : { y: d.getFullYear(), mo: d.getMonth() + 1, d: d.getDate(), h: d.getHours(), mi: d.getMinutes(), s: d.getSeconds(), ms: d.getMilliseconds() }
}

function parse(iso: string | null | undefined): Date | null {
  if (!iso) return null
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? null : d
}

/** `14:03:09` (or with milliseconds). */
export function clock(iso: string | null | undefined, tz: TimeZoneMode, ms = false): string {
  const d = parse(iso)
  if (!d) return '—'
  const p = parts(d, tz)
  const base = `${pad(p.h)}:${pad(p.mi)}:${pad(p.s)}`
  return ms ? `${base}.${pad(p.ms, 3)}` : base
}

/** `2026-11-25 14:03:09` plus ` UTC` in UTC mode. */
export function dateTime(iso: string | null | undefined, tz: TimeZoneMode): string {
  const d = parse(iso)
  if (!d) return '—'
  const p = parts(d, tz)
  return `${p.y}-${pad(p.mo)}-${pad(p.d)} ${pad(p.h)}:${pad(p.mi)}:${pad(p.s)}${tz === 'utc' ? ' UTC' : ''}`
}

/** Short form for tables: time today, date + time otherwise. */
export function shortTime(iso: string | null | undefined, tz: TimeZoneMode, now = Date.now()): string {
  const d = parse(iso)
  if (!d) return '—'
  const p = parts(d, tz)
  const n = parts(new Date(now), tz)
  const sameDay = p.y === n.y && p.mo === n.mo && p.d === n.d
  return sameDay
    ? `${pad(p.h)}:${pad(p.mi)}:${pad(p.s)}`
    : `${pad(p.mo)}-${pad(p.d)} ${pad(p.h)}:${pad(p.mi)}`
}

/** `3 min ago`, `2 h ago`, `in the future` (clock skew). */
export function relative(iso: string | null | undefined, now = Date.now()): string {
  const d = parse(iso)
  if (!d) return '—'
  const s = Math.round((now - d.getTime()) / 1000)
  if (s < -5) return 'in the future'
  if (s < 10) return 'just now'
  if (s < 60) return `${s} s ago`
  const m = Math.round(s / 60)
  if (m < 60) return `${m} min ago`
  const h = Math.round(m / 60)
  if (h < 48) return `${h} h ago`
  return `${Math.round(h / 24)} d ago`
}

/** `850 ms`, `4.2 s`, `3 m 12 s`, `1 h 05 m`. */
export function duration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms) || ms < 0) return '—'
  if (ms < 1000) return `${Math.round(ms)} ms`
  const s = ms / 1000
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} m ${pad(Math.floor(s % 60))} s`
  const h = Math.floor(m / 60)
  return `${h} h ${pad(m % 60)} m`
}

/** Milliseconds between two timestamps, or null. */
export function between(a: string | null | undefined, b: string | null | undefined): number | null {
  const x = parse(a)
  const y = parse(b)
  return x && y ? y.getTime() - x.getTime() : null
}

export function count(n: number | null | undefined): string {
  if (n === null || n === undefined) return '—'
  return n.toLocaleString('en-US')
}

/** `1.2k`, `3.4M` for dense stat cards; exact below 10,000. */
export function compact(n: number | null | undefined): string {
  if (n === null || n === undefined) return '—'
  if (n < 10_000) return n.toLocaleString('en-US')
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 100_000 ? 1 : 0)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

export function bytes(n: number | null | undefined): string {
  if (n === null || n === undefined) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}

/** Last two path segments, for narrow cells. The full path goes in a title. */
export function shortPath(p: string | null | undefined, segments = 2): string {
  if (!p) return '—'
  const norm = p.replace(/\\/g, '/')
  const bits = norm.split('/').filter(Boolean)
  if (bits.length <= segments) return norm
  return `…/${bits.slice(-segments).join('/')}`
}

/** Path relative to a project root when it lies inside it. */
export function relativeTo(path: string, root: string | null | undefined): string {
  if (!root) return path
  const r = root.endsWith('/') ? root : `${root}/`
  return path.startsWith(r) ? path.slice(r.length) : path
}

/** Last segment of a project path, used as its display name. */
export function projectName(p: string | null | undefined): string {
  if (!p) return 'no project'
  const bits = p.replace(/\\/g, '/').split('/').filter(Boolean)
  return bits[bits.length - 1] ?? p
}

export function shortId(id: string | null | undefined, n = 8): string {
  if (!id) return '—'
  const tail = id.includes(':') ? id.slice(id.indexOf(':') + 1) : id
  return tail.length > n ? tail.slice(0, n) : tail
}
