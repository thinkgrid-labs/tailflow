import { useMemo, useRef, useEffect, useState, useCallback } from 'preact/hooks'
import { useLogStream } from './hooks/useLogStream'
import { LogRow } from './components/LogRow'
import { Sidebar } from './components/Sidebar'
import type { LogLevel } from './types'
import { LEVEL_COLOR } from './types'

const ALL_LEVELS: LogLevel[] = ['error', 'warn', 'info', 'debug', 'trace', 'unknown']

function tryRegex(s: string): RegExp | null {
  try { return new RegExp(s, 'i') } catch { return null }
}

export function App() {
  const { records, status, clear } = useLogStream()

  // ── Filter state ──────────────────────────────────────────────────────────
  const [filter,          setFilter]          = useState('')
  const [selectedSources, setSelectedSources] = useState<Set<string> | null>(null)
  const [selectedLevels,  setSelectedLevels]  = useState<Set<LogLevel> | null>(null)

  // ── Source map (name → total count) ───────────────────────────────────────
  const sources = useMemo(() => {
    const m = new Map<string, number>()
    for (const r of records) m.set(r.source, (m.get(r.source) ?? 0) + 1)
    return m
  }, [records])

  // ── Filtered view ─────────────────────────────────────────────────────────
  const filtered = useMemo(() => {
    const re = filter ? tryRegex(filter) : null
    const lower = filter.toLowerCase()
    return records.filter(r => {
      if (selectedSources && !selectedSources.has(r.source)) return false
      if (selectedLevels  && !selectedLevels.has(r.level))   return false
      if (filter) {
        return re
          ? re.test(r.payload) || re.test(r.source)
          : r.payload.toLowerCase().includes(lower) || r.source.toLowerCase().includes(lower)
      }
      return true
    })
  }, [records, filter, selectedSources, selectedLevels])

  // ── Auto-scroll ───────────────────────────────────────────────────────────
  const feedRef    = useRef<HTMLDivElement>(null)
  const sentinelRef = useRef<HTMLDivElement>(null)
  const [atBottom, setAtBottom] = useState(true)

  useEffect(() => {
    const el = feedRef.current
    if (!el) return
    const ob = new IntersectionObserver(
      ([entry]) => setAtBottom(entry.isIntersecting),
      { root: el, threshold: 0.1 }
    )
    if (sentinelRef.current) ob.observe(sentinelRef.current)
    return () => ob.disconnect()
  }, [])

  useEffect(() => {
    if (atBottom && sentinelRef.current) {
      sentinelRef.current.scrollIntoView({ behavior: 'instant', block: 'end' })
    }
  }, [filtered, atBottom])

  const scrollToBottom = useCallback(() => {
    sentinelRef.current?.scrollIntoView({ behavior: 'smooth', block: 'end' })
    setAtBottom(true)
  }, [])

  // ── Source sidebar toggles ────────────────────────────────────────────────
  const toggleSource = useCallback((name: string) => {
    setSelectedSources(prev => {
      if (prev === null) {
        // deselect all except this one
        return new Set([name])
      }
      const next = new Set(prev)
      if (next.has(name)) {
        next.delete(name)
        return next.size === 0 ? null : next
      } else {
        next.add(name)
        return next
      }
    })
  }, [])

  const selectAllSources = useCallback(() => setSelectedSources(null), [])

  // ── Level pill toggles ────────────────────────────────────────────────────
  const toggleLevel = useCallback((level: LogLevel) => {
    setSelectedLevels(prev => {
      if (prev === null) return new Set(ALL_LEVELS.filter(l => l !== level))
      const next = new Set(prev)
      if (next.has(level)) {
        next.delete(level)
        return next.size === 0 ? null : next
      } else {
        next.add(level)
        if (next.size === ALL_LEVELS.length) return null   // all on → reset
        return next
      }
    })
  }, [])

  const statusDot = status === 'connected' ? '●' : status === 'connecting' ? '◌' : '○'
  const statusCls = `status-dot status-dot--${status}`

  return (
    <div class="layout">
      {/* ── Header ──────────────────────────────────────────────────────── */}
      <header class="header">
        <span class="logo">TailFlow</span>

        <input
          class="filter-input"
          type="text"
          placeholder="filter by keyword or /regex/"
          value={filter}
          onInput={e => setFilter((e.target as HTMLInputElement).value)}
          spellcheck={false}
        />

        <div class="level-pills">
          {ALL_LEVELS.filter(l => l !== 'unknown').map(level => {
            const active = selectedLevels === null || selectedLevels.has(level)
            return (
              <button
                key={level}
                class={`level-pill ${active ? 'level-pill--active' : 'level-pill--dim'}`}
                style={{ '--lc': LEVEL_COLOR[level] } as never}
                onClick={() => toggleLevel(level)}
              >
                {level.slice(0, 3).toUpperCase()}
              </button>
            )
          })}
        </div>

        <span class={statusCls} title={status}>{statusDot}</span>
        <span class="record-count">{filtered.length.toLocaleString()}</span>
        <button class="clear-btn" onClick={clear} title="Clear all logs">✕</button>
      </header>

      {/* ── Body ────────────────────────────────────────────────────────── */}
      <div class="body">
        <Sidebar
          sources={sources}
          selected={selectedSources}
          onToggle={toggleSource}
          onSelectAll={selectAllSources}
        />

        <div class="feed" ref={feedRef}>
          {filtered.map((r, i) => <LogRow key={i} record={r} />)}
          <div ref={sentinelRef} class="sentinel" />

          {!atBottom && (
            <button class="scroll-btn" onClick={scrollToBottom} title="Scroll to bottom">
              ↓ latest
            </button>
          )}
        </div>
      </div>
    </div>
  )
}
