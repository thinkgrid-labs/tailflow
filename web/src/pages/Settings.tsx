import { Fragment } from 'preact'
import { useSettings } from '../lib/hooks.ts'
import { dateTime } from '../lib/format.ts'
import { Card, Segmented } from '../components/ui'
import { signOut } from '../lib/auth.ts'

const SHORTCUTS: [string, string][] = [
  ['/', 'Focus search'],
  ['j / k', 'Next / previous event in a list'],
  ['Enter', 'Open the focused row'],
  ['Esc', 'Close the event inspector'],
  ['+ / −', 'Zoom the focused timeline'],
  ['← / →', 'Pan the focused timeline'],
  ['Shift + drag', 'Select a time range on the timeline'],
]

export function SettingsPage() {
  const { settings, update } = useSettings()
  const sample = new Date().toISOString()
  return (
    <div class="page" style={{ maxWidth: '820px' }}>
      <div class="page-head">
        <div>
          <h2>Settings</h2>
          <p>Display preferences for this browser. They are stored locally in this browser and never sent anywhere.</p>
        </div>
      </div>
      <Card title="Display">
        <dl class="kv" style={{ alignItems: 'center', rowGap: '14px' }}>
          <dt>Theme</dt>
          <dd><Segmented label="Theme" value={settings.theme} onChange={theme => update({ theme })}
            options={[{ value: 'system', label: 'System' }, { value: 'dark', label: 'Dark' }, { value: 'light', label: 'Light' }]} /></dd>
          <dt>Time zone</dt>
          <dd>
            <Segmented label="Time zone" value={settings.tz} onChange={tz => update({ tz })}
              options={[{ value: 'local', label: 'Local' }, { value: 'utc', label: 'UTC' }]} />
            <div class="note" style={{ marginTop: '4px' }}>Now: {dateTime(sample, settings.tz)}</div>
          </dd>
          <dt>Density</dt>
          <dd><Segmented label="Density" value={settings.density} onChange={density => update({ density })}
            options={[{ value: 'comfortable', label: 'Comfortable' }, { value: 'compact', label: 'Compact' }]} /></dd>
          <dt>Log lines</dt>
          <dd><Segmented label="Log line wrapping" value={settings.wrap ? 'wrap' : 'scroll'} onChange={v => update({ wrap: v === 'wrap' })}
            options={[{ value: 'scroll', label: 'Scroll horizontally' }, { value: 'wrap', label: 'Wrap' }]} /></dd>
        </dl>
      </Card>
      <Card title="Keyboard">
        <dl class="kv">
          {SHORTCUTS.map(([k, v]) => (<Fragment key={k}><dt><kbd class="mono">{k}</kbd></dt><dd>{v}</dd></Fragment>))}
        </dl>
      </Card>
      <Card title="Access">
        <p class="note">
          This browser is signed in to the daemon with a session cookie that only this dashboard's origin can use.
          It survives daemon restarts on this port and ends after 12 hours without use, after 7 days, when you sign out,
          or when someone runs <code>tailflow-daemon dashboard --revoke-sessions</code>.
        </p>
        <button class="btn" onClick={() => void signOut()}>Sign out of this daemon</button>
      </Card>
      <Card title="About this dashboard">
        <p class="note">
          Served by your local TailFlow daemon and talks only to it, through read-only endpoints. No analytics, no external fonts or scripts, no
          network requests beyond this machine. Live logs stream over Server-Sent Events; agent activity is read from durable history and refreshed while the page is visible.
        </p>
      </Card>
    </div>
  )
}
