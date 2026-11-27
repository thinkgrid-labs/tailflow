// A variable-height virtual list. Renders only the rows in view plus a
// bounded overscan, measures real heights with ResizeObserver, and keeps
// the viewport stable while rows are added, trimmed from the head, or
// expanded above it.

import type { ComponentChildren } from 'preact'
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks'
import { HeightIndex } from '../lib/virtual.ts'

export interface VirtualHandle {
  scrollToBottom: () => void
  scrollToKey: (key: string | number) => void
}

interface Props<T> {
  items: readonly T[]
  keyOf: (item: T) => string | number
  render: (item: T, index: number) => ComponentChildren
  estimate: number
  overscan?: number
  /** Keep the newest row in view as items arrive. */
  follow?: boolean
  onFollowChange?: (atBottom: boolean) => void
  /** Lines never wrap: track the widest row seen for a stable scroll width. */
  noWrap?: boolean
  onEndReached?: () => void
  handle?: (h: VirtualHandle) => void
  label: string
  class?: string
  /** Rendered after the rows inside the scroller (e.g. "Load more"). */
  footer?: ComponentChildren
}

/** After the user leaves the tail, "at the bottom" is not a return for this long. */
const LEAVE_GRACE_MS = 400

export function VirtualList<T>(p: Props<T>) {
  const scroller = useRef<HTMLDivElement>(null)
  const index = useRef(new HeightIndex(p.estimate)).current
  const [scrollTop, setScrollTop] = useState(0)
  const [viewport, setViewport] = useState(600)
  const [, force] = useState(0)
  const [maxWidth, setMaxWidth] = useState(0)
  const prevKeys = useRef<(string | number)[]>([])
  const pendingAdjust = useRef(0)
  const firstVisible = useRef(0)
  const raf = useRef<number | null>(null)
  /** scrollTop we set while following; only moving above it is a user leaving the tail. */
  const pinned = useRef<number | null>(null)
  /** When the user last left the tail by input (wheel up, PageUp, scrollbar). */
  const leftAt = useRef(0)

  const keys = useMemo(() => p.items.map(p.keyOf), [p.items])

  // Head trimming: if the old list's rows before our new first row were
  // removed, the content above the viewport shrank by their height.
  if (keys !== prevKeys.current) {
    const old = prevKeys.current
    if (!p.follow && old.length > 0 && keys.length > 0 && old[0] !== keys[0]) {
      const i = old.indexOf(keys[0])
      if (i > 0) pendingAdjust.current -= index.heightOfFirst(i)
    }
    index.setKeys(keys)
    index.prune()
    prevKeys.current = keys
  }

  useEffect(() => {
    index.estimate = p.estimate
  }, [p.estimate])

  // Viewport size.
  useEffect(() => {
    const el = scroller.current
    if (!el) return
    const ro = new ResizeObserver(() => setViewport(el.clientHeight))
    ro.observe(el)
    setViewport(el.clientHeight)
    return () => ro.disconnect()
  }, [])

  // Row measurement. One observer for all rendered rows.
  const rowObserver = useMemo(() => new ResizeObserver(entries => {
    let changed = false
    let above = 0
    for (const entry of entries) {
      const el = entry.target as HTMLElement
      const raw = el.dataset.key
      if (raw === undefined) continue
      const key = el.dataset.num === '1' ? Number(raw) : raw
      const before = index.heightOf(key)
      const h = el.getBoundingClientRect().height
      if (index.measure(key, h)) {
        changed = true
        const i = Number(el.dataset.index)
        if (i < firstVisible.current) above += h - before
      }
    }
    if (p.noWrap) {
      let w = 0
      for (const entry of entries) w = Math.max(w, (entry.target as HTMLElement).scrollWidth)
      if (w > 0) setMaxWidth(m => (w > m ? w : m))
    }
    if (changed) {
      // A row above the viewport changed height: shift by the same amount so
      // what the user is reading does not move.
      if (above !== 0 && !p.follow) pendingAdjust.current += above
      force(n => n + 1)
    }
  }), [p.noWrap, p.follow])
  useEffect(() => () => rowObserver.disconnect(), [rowObserver])

  useEffect(() => { setMaxWidth(0) }, [p.noWrap])

  const total = index.total()
  const range = index.range(scrollTop, viewport, p.overscan ?? 8)
  firstVisible.current = index.indexAt(scrollTop)
  const top = index.offsetOf(range.start)

  useLayoutEffect(() => {
    const el = scroller.current
    if (!el) return
    if (p.follow) {
      const target = Math.max(0, total - el.clientHeight)
      if (Math.abs(el.scrollTop - target) > 1) el.scrollTop = target
      pinned.current = el.scrollTop
      pendingAdjust.current = 0
    } else if (pendingAdjust.current !== 0) {
      el.scrollTop = Math.max(0, el.scrollTop + pendingAdjust.current)
      pendingAdjust.current = 0
    }
  })

  const onScroll = () => {
    if (raf.current !== null) return
    raf.current = requestAnimationFrame(() => {
      raf.current = null
      const el = scroller.current
      if (!el) return
      setScrollTop(el.scrollTop)
      const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 4
      if (p.follow) {
        // Our own scroll-to-bottom also fires scroll events, and new rows can
        // grow the content before this frame reads it: being "not at the
        // bottom" then is not the user leaving. Only an upward move is.
        const movedUp = pinned.current !== null && el.scrollTop < pinned.current - 4
        if (movedUp) p.onFollowChange?.(false)
      } else if (!atBottom || performance.now() - leftAt.current > LEAVE_GRACE_MS) {
        // Right after the user left, a re-pin already queued (follow was
        // still on when it rendered) can put the view back at the bottom
        // before the user's own scroll applies (seen in WebKit under a fast
        // stream). Being at the bottom then is not the user returning.
        p.onFollowChange?.(atBottom)
      }
      if (p.onEndReached && el.scrollTop + el.clientHeight >= el.scrollHeight - 200) p.onEndReached()
    })
  }
  useEffect(() => () => { if (raf.current !== null) cancelAnimationFrame(raf.current) }, [])

  // Leaving the tail is decided from the user's input, not from positions:
  // under a fast stream the next update re-pins the view before a scroll
  // event could be compared with it.
  const leave = () => {
    leftAt.current = performance.now()
    if (p.follow) p.onFollowChange?.(false)
  }
  const onWheel = (e: WheelEvent) => { if (e.deltaY < 0) leave() }
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'PageUp' || e.key === 'ArrowUp' || e.key === 'Home') leave()
  }
  const onPointerDown = (e: PointerEvent) => {
    // A press on the scroller itself (not a row) is a scrollbar drag.
    if (e.target === scroller.current) leave()
  }

  useEffect(() => {
    p.handle?.({
      scrollToBottom: () => {
        const el = scroller.current
        if (el) el.scrollTop = el.scrollHeight
      },
      scrollToKey: key => {
        const el = scroller.current
        const i = keys.indexOf(key)
        if (el && i >= 0) el.scrollTop = Math.max(0, index.offsetOf(i) - el.clientHeight / 3)
      },
    })
  })

  const rows: ComponentChildren[] = []
  for (let i = range.start; i < range.end; i++) {
    const item = p.items[i]
    const key = keys[i]
    rows.push(
      <Row key={key} k={key} i={i} observer={rowObserver}>
        {p.render(item, i)}
      </Row>,
    )
  }

  return (
    <div class={`vlist${p.class ? ` ${p.class}` : ''}`} ref={scroller} onScroll={onScroll} onWheel={onWheel}
      onKeyDown={onKeyDown} onPointerDown={onPointerDown} onTouchMove={leave} role="list" aria-label={p.label} tabIndex={0}>
      <div class="vlist-inner" style={{ height: `${total}px`, minWidth: maxWidth ? `${maxWidth}px` : undefined }}>
        <div class="vlist-rows" style={{ transform: `translateY(${top}px)`, width: p.noWrap ? 'max-content' : undefined, minWidth: '100%' }}>
          {rows}
        </div>
      </div>
      {/* Inside role=list, so the footer ("load more") is a list item too. */}
      {p.footer && <div role="listitem">{p.footer}</div>}
    </div>
  )
}

/** One rendered row: observed while mounted, unobserved on unmount, so the
 *  observer never retains rows that scrolled out of the window. */
function Row({ k, i, observer, children }: {
  k: string | number; i: number; observer: ResizeObserver; children: ComponentChildren
}) {
  const el = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    const node = el.current
    if (!node) return
    observer.observe(node)
    return () => observer.unobserve(node)
  }, [observer])
  return (
    <div ref={el} role="listitem" data-key={String(k)} data-num={typeof k === 'number' ? '1' : '0'} data-index={i}>
      {children}
    </div>
  )
}
