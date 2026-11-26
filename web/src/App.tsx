import { Fragment } from 'preact'
import { useEffect, useRef, useState } from 'preact/hooks'
import { useRoute, SettingsContext, navigate, useAgentStreamStatus } from './lib/hooks.ts'
import { href, type Page } from './lib/router.ts'
import { applySettings, loadSettings, saveSettings, type Settings } from './lib/settings.ts'
import { useLiveStatus } from './hooks/useLogStream'
import { Icon, type IconName } from './components/icons'
import { Overview } from './pages/Overview'
import { Sessions } from './pages/Sessions'
import { SessionDetailPage } from './pages/SessionDetail'
import { Activity } from './pages/Activity'
import { Timeline } from './pages/Timeline'
import { Logs } from './pages/Logs'
import { Files } from './pages/Files'
import { Commands } from './pages/Commands'
import { Integrations } from './pages/Integrations'
import { SettingsPage } from './pages/Settings'
import { SignIn } from './components/SignIn'
import { authState, checkSession, subscribeAuth, type AuthState } from './lib/auth.ts'

const NAV: { page: Page; label: string; icon: IconName; section?: string }[] = [
  { page: 'overview', label: 'Overview', icon: 'overview' },
  { page: 'sessions', label: 'Sessions', icon: 'sessions' },
  { page: 'activity', label: 'Activity', icon: 'activity' },
  { page: 'timeline', label: 'Timeline', icon: 'timeline' },
  { page: 'logs', label: 'Logs', icon: 'logs' },
  { page: 'errors', label: 'Errors', icon: 'errors' },
  { page: 'files', label: 'Files', icon: 'files', section: 'Operations' },
  { page: 'commands', label: 'Commands', icon: 'commands' },
  { page: 'integrations', label: 'Integrations', icon: 'integrations', section: 'System' },
  { page: 'settings', label: 'Settings', icon: 'settings' },
]

const TITLES: Record<Page, string> = {
  overview: 'Overview', sessions: 'Sessions', session: 'Session', activity: 'Activity',
  timeline: 'Timeline', logs: 'Logs', errors: 'Errors', files: 'Files', commands: 'Commands',
  integrations: 'Integrations & storage', settings: 'Settings',
}

/** Agent stream state → the connection pill's tone and label. Pages fall
 *  back to polling whenever the stream is not live. */
const AGENT_CONN = { live: 'connected', connecting: 'connecting', reconnecting: 'connecting', off: 'disconnected' } as const
const AGENT_LABEL = { live: 'Live', connecting: 'Connecting', reconnecting: 'Reconnecting', off: 'Polling' } as const

/** Checks the browser session first: nothing that reads data (pages, the
 *  live stream) mounts until the daemon accepts it. */
export function App() {
  const [auth, setAuth] = useState<AuthState>(authState)
  useEffect(() => {
    const off = subscribeAuth(() => setAuth(authState()))
    void checkSession()
    return off
  }, [])
  useEffect(() => applySettings(loadSettings()), [])
  if (auth.status === 'signed_out') {
    document.title = 'Sign in · TailFlow'
    return <SignIn reason={auth.reason} />
  }
  if (auth.status === 'checking') return <div class="signin" aria-busy="true" />
  return <Dashboard />
}

function Dashboard() {
  const [settings, setSettings] = useState<Settings>(loadSettings)
  const update = (p: Partial<Settings>) => setSettings(s => {
    const next = { ...s, ...p }
    saveSettings(next)
    return next
  })
  useEffect(() => applySettings(settings), [settings])

  const route = useRoute()
  const status = useLiveStatus()
  const agentStatus = useAgentStreamStatus()
  const [navOpen, setNavOpen] = useState(false)
  const [version, setVersion] = useState<string | null>(null)
  const search = useRef<HTMLInputElement>(null)
  const main = useRef<HTMLElement>(null)

  useEffect(() => {
    fetch('/health').then(r => (r.ok ? r.json() : null))
      .then((h: { version?: unknown } | null) => { if (h && typeof h.version === 'string') setVersion(h.version) })
      .catch(() => {})
  }, [])

  // Page change: close the mobile drawer, reset scroll, set the title.
  useEffect(() => {
    setNavOpen(false)
    main.current?.scrollTo(0, 0)
    document.title = `${TITLES[route.page]} · TailFlow`
  }, [route.page, route.id])

  // `/` focuses search (unless typing somewhere already).
  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement
      if (e.key === '/' && !e.metaKey && !e.ctrlKey && !/^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName)) {
        e.preventDefault()
        search.current?.focus()
      }
    }
    window.addEventListener('keydown', on)
    return () => window.removeEventListener('keydown', on)
  }, [])

  const active = route.page === 'session' ? 'sessions' : route.page
  const statusLabel = status === 'connected' ? 'Live' : status === 'connecting' ? 'Reconnecting' : 'Disconnected'

  return (
    <SettingsContext.Provider value={{ settings, update }}>
      <a class="skip-link" href="#main" onClick={e => { e.preventDefault(); main.current?.focus() }}>Skip to content</a>
      <div class={`app${settings.sidebarCollapsed ? ' collapsed' : ''}${navOpen ? ' nav-open' : ''}`} onClick={e => {
        if (navOpen && (e.target as HTMLElement).classList.contains('app')) setNavOpen(false)
      }}>
        <nav class="sidebar" aria-label="Main">
          <div class="brand">
            <span class="brand-mark" aria-hidden="true"><Icon name="logo" size="sm" /></span>
            <span class="brand-word">TailFlow</span>
            {version && <span class="brand-ver">v{version}</span>}
          </div>
          {NAV.map(n => (
            <Fragment key={n.page}>
              {n.section && <div class="nav-section">{n.section}</div>}
              <a class="nav-item" href={href(n.page)} aria-current={active === n.page ? 'page' : undefined}
                title={settings.sidebarCollapsed ? n.label : undefined}>
                <Icon name={n.icon} />
                <span class="nav-label">{n.label}</span>
              </a>
            </Fragment>
          ))}
          <div class="sidebar-foot">
            <button class="nav-item hide-sm" style={{ width: '100%' }} onClick={() => update({ sidebarCollapsed: !settings.sidebarCollapsed })}
              aria-label={settings.sidebarCollapsed ? 'Expand sidebar' : 'Collapse sidebar'} aria-expanded={!settings.sidebarCollapsed}>
              <Icon name="panel" />
              <span class="nav-label">Collapse</span>
            </button>
          </div>
        </nav>
        <header class="topbar">
          <button class="btn icon-only ghost menu-btn" onClick={() => setNavOpen(o => !o)} aria-label="Open navigation" aria-expanded={navOpen}>
            <Icon name="menu" />
          </button>
          <h1>{TITLES[route.page]}</h1>
          <div class="spacer" />
          <form class="global-search" role="search" onSubmit={e => {
            e.preventDefault()
            const q = search.current?.value.trim() ?? ''
            navigate(href('activity', { q }))
          }}>
            <Icon name="search" size="sm" />
            <input ref={search} class="input" type="search" placeholder="Search all history  ( / )" aria-label="Search all history" />
          </form>
          <span class={`conn conn--${AGENT_CONN[agentStatus]}`} role="status" title={`Agent event stream: ${AGENT_LABEL[agentStatus].toLowerCase()}`}>
            <span class="conn-dot" aria-hidden="true" />
            <span class="conn-label">Agents {AGENT_LABEL[agentStatus].toLowerCase()}</span>
          </span>
          <span class={`conn conn--${status}`} role="status" title={`Live log stream: ${statusLabel.toLowerCase()}`}>
            <span class="conn-dot" aria-hidden="true" />
            <span class="conn-label">{statusLabel}</span>
          </span>
        </header>
        <main class="main" id="main" ref={main} tabIndex={-1}>
          <PageView />
        </main>
      </div>
    </SettingsContext.Provider>
  )
}

function PageView() {
  const route = useRoute()
  switch (route.page) {
    case 'overview': return <Overview />
    case 'sessions': return <Sessions params={route.params} />
    case 'session': return <SessionDetailPage id={route.id ?? ''} params={route.params} />
    case 'activity': return <Activity params={route.params} mode="activity" />
    case 'errors': return <Activity params={route.params} mode="errors" />
    case 'timeline': return <Timeline params={route.params} />
    case 'logs': return <Logs />
    case 'files': return <Files params={route.params} />
    case 'commands': return <Commands params={route.params} />
    case 'integrations': return <Integrations />
    case 'settings': return <SettingsPage />
  }
}
