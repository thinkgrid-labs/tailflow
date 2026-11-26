import { Icon } from './icons'
import { CopyButton } from './ui'
import { signInCommand } from '../lib/auth.ts'

/** Shown instead of the dashboard when this browser has no valid session. */
export function SignIn({ reason }: { reason: string | null }) {
  const cmd = signInCommand(location.port)
  const why = reason === 'session_expired'
    ? 'Your browser session ended: it was unused for 12 hours, reached its 7-day limit, or was revoked.'
    : reason === 'signed_out'
      ? 'You signed out of this daemon.'
      : reason === 'cross_origin'
        ? 'The daemon refused a request that did not come from this dashboard.'
        : 'This browser is not signed in to this TailFlow daemon yet.'
  return (
    <main class="signin" id="main" tabIndex={-1}>
      <div class="card signin-card">
        <div class="card-body">
          <div class="brand signin-brand">
            <span class="brand-mark" aria-hidden="true"><Icon name="logo" size="sm" /></span>
            <span class="brand-word">TailFlow</span>
          </div>
          <h1 class="signin-title">Sign in to this daemon</h1>
          <p class="signin-text">{why}</p>
          <p class="signin-text">Run this in a terminal on this machine, as the user that started the daemon:</p>
          <div class="signin-cmd">
            <code>{cmd}</code>
            <CopyButton text={cmd} />
          </div>
          <p class="signin-text muted">
            It opens a one-time sign-in link, valid for 5 minutes. The link printed when the daemon
            started also works, once, for an hour.
          </p>
          <div class="signin-actions">
            <button class="btn" onClick={() => location.reload()}>I've signed in, reload</button>
          </div>
          <p class="signin-text muted small">
            Why: the dashboard shows agent sessions, project and file paths, commands and logs.
            A credential keeps other OS users, containers and web pages on this machine from reading them.
          </p>
        </div>
      </div>
    </main>
  )
}
