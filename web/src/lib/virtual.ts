// Variable-height list virtualization math. Rows are identified by a
// stable key; measured heights survive re-ordering, trimming and filtering.
// Pure: the component feeds in measurements and reads back offsets.

export interface Range {
  start: number
  /** Exclusive. */
  end: number
}

export class HeightIndex {
  private measured = new Map<string | number, number>()
  private offsets = new Float64Array(1)
  private keys: readonly (string | number)[] = []
  private dirty = true

  estimate: number

  constructor(estimate: number) {
    this.estimate = estimate
  }

  setKeys(keys: readonly (string | number)[]): void {
    if (keys !== this.keys) {
      this.keys = keys
      this.dirty = true
    }
  }

  /** Record a measured height. Returns true when it changed layout. */
  measure(key: string | number, height: number): boolean {
    if (!(height > 0)) return false
    const prev = this.measured.get(key)
    if (prev !== undefined && Math.abs(prev - height) < 0.5) return false
    this.measured.set(key, height)
    this.dirty = true
    return true
  }

  heightOf(key: string | number): number {
    return this.measured.get(key) ?? this.estimate
  }

  /** Drop measurements for keys no longer present (bounded memory). */
  prune(): void {
    if (this.measured.size <= this.keys.length * 2 + 256) return
    const live = new Set(this.keys)
    for (const k of this.measured.keys()) if (!live.has(k)) this.measured.delete(k)
  }

  private rebuild(): void {
    if (!this.dirty) return
    const n = this.keys.length
    if (this.offsets.length < n + 1) this.offsets = new Float64Array(Math.max(n + 1, this.offsets.length * 2))
    this.offsets[0] = 0
    for (let i = 0; i < n; i++) this.offsets[i + 1] = this.offsets[i] + this.heightOf(this.keys[i])
    this.dirty = false
  }

  get count(): number { return this.keys.length }

  total(): number {
    this.rebuild()
    return this.offsets[this.keys.length]
  }

  offsetOf(index: number): number {
    this.rebuild()
    const i = Math.max(0, Math.min(index, this.keys.length))
    return this.offsets[i]
  }

  /** Index of the row containing `y` (clamped). */
  indexAt(y: number): number {
    this.rebuild()
    const n = this.keys.length
    if (n === 0) return 0
    let lo = 0
    let hi = n - 1
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1
      if (this.offsets[mid] <= y) lo = mid
      else hi = mid - 1
    }
    return lo
  }

  /** Rows intersecting `[scrollTop, scrollTop + viewport)` plus overscan rows on each side. */
  range(scrollTop: number, viewport: number, overscan: number): Range {
    const n = this.keys.length
    if (n === 0) return { start: 0, end: 0 }
    const first = this.indexAt(Math.max(0, scrollTop))
    const last = this.indexAt(Math.max(0, scrollTop + viewport))
    return {
      start: Math.max(0, first - overscan),
      end: Math.min(n, last + 1 + overscan),
    }
  }

  /** Height of the first `n` rows (what disappears when they are trimmed). */
  heightOfFirst(n: number): number {
    return this.offsetOf(n)
  }
}
