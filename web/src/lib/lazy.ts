// Route-level code splitting without preact/compat: a page module is
// loaded the first time its route renders. The CSP (`script-src 'self'`)
// allows the chunk; nothing is fetched from anywhere else.

import { h, type ComponentType, type FunctionComponent } from 'preact'
import { useEffect, useState } from 'preact/hooks'

export function lazy<P extends object>(load: () => Promise<ComponentType<P>>, fallback: () => preact.JSX.Element): FunctionComponent<P> {
  let loaded: ComponentType<P> | null = null
  let pending: Promise<ComponentType<P>> | null = null
  return (props: P) => {
    // Hooks run on every render, so their order never changes.
    const [c, setC] = useState<ComponentType<P> | null>(() => loaded)
    useEffect(() => {
      if (c) return
      let live = true
      pending ??= load().then(x => (loaded = x))
      void pending.then(x => { if (live) setC(() => x) })
      return () => { live = false }
    }, [c])
    return c ? h(c, props) : fallback()
  }
}
