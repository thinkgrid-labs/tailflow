// Local API access control (M4-A) in a real browser.
//
// The threat: another web application on this machine's loopback (a dev
// server on 127.0.0.1:3000, a compromised npm package's page, another
// tool's UI) running script in the same browser that is signed in to the
// TailFlow dashboard. Cookies are not isolated by port, so the browser
// attaches the TailFlow session cookie to that page's requests.

import { test, expect } from '@playwright/test'
import http from 'node:http'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { freePort, startDaemon } from './daemon'
// @ts-expect-error plain ESM helper
import { authHeaders } from './fixture.mjs'

const state = () => JSON.parse(fs.readFileSync(path.join(os.tmpdir(), 'tailflow-e2e-state.json'), 'utf8')) as { base: string; sessions: Record<string, string> }

/** Serve `html` on 127.0.0.1:<port>; records the Cookie headers it receives. */
async function attackerServer(html: string) {
  const port = await freePort()
  const cookies: string[] = []
  const server = http.createServer((req, res) => {
    if (req.headers.cookie) cookies.push(req.headers.cookie)
    res.writeHead(200, { 'content-type': 'text/html' })
    res.end(html)
  })
  await new Promise<void>(r => server.listen(port, '127.0.0.1', () => r()))
  // Browsers keep loopback connections alive (WebKit for ~30 s): drop them,
  // or close() waits for them.
  return { port, cookies, close: () => new Promise<void>(r => { server.close(() => r()); server.closeAllConnections() }) }
}

/** Every read a page can attempt against the daemon, run inside the page. */
const ATTACK = `<!doctype html><html><body><script>
window.attack = async (base, sessionId) => {
  const out = {}
  const tryFetch = async (url, init) => {
    try { const r = await fetch(url, init); return { status: r.status, type: r.type, body: r.type === 'opaque' ? null : await r.text() } }
    catch (e) { return { error: String(e) } }
  }
  out.corsFetch = await tryFetch(base + '/api/v1/sessions', { credentials: 'include' })
  out.corsHealth = await tryFetch(base + '/health', { credentials: 'include' })
  out.noCors = await tryFetch(base + '/api/v1/sessions', { mode: 'no-cors', credentials: 'include' })
  out.sse = await new Promise(resolve => {
    const es = new EventSource(base + '/events', { withCredentials: true })
    let got = null
    es.onmessage = e => { got = e.data }
    es.onerror = () => { es.close(); resolve({ error: true, got }) }
    setTimeout(() => { es.close(); resolve({ timeout: true, got }) }, 4000)
  })
  const probe = (tag, url) => new Promise(resolve => {
    const el = document.createElement(tag)
    el.onload = () => resolve('load')
    el.onerror = () => resolve('error')
    el.src = url
    document.body.appendChild(el)
    setTimeout(() => resolve('timeout'), 4000)
  })
  // Existence oracle: an existing and a missing session must look the same.
  out.scriptExisting = await probe('script', base + '/api/v1/sessions/' + encodeURIComponent(sessionId))
  out.scriptMissing = await probe('script', base + '/api/v1/sessions/claude-code%3Anope')
  out.imgExisting = await probe('img', base + '/api/v1/sessions/' + encodeURIComponent(sessionId))
  out.imgMissing = await probe('img', base + '/api/v1/sessions/claude-code%3Anope')
  return out
}
</script></body></html>`

for (const host of ['127.0.0.1', 'localhost']) {
  test(`a page on another loopback port (${host}) cannot read the API, even with the session cookie`, async ({ page, context }) => {
    const { base, sessions } = state()
    const before = (await (await fetch(`${base}/health`, { headers: authHeaders })).json()).auth.refused_requests as number
    // The browser is signed in to the dashboard (storageState).
    expect((await context.cookies(base)).some(c => c.name.startsWith('tailflow_session_'))).toBe(true)

    const evil = await attackerServer(ATTACK)
    try {
      await page.goto(`http://${host}:${evil.port}/`)
      const out = await page.evaluate(([b, s]) => (window as any).attack(b, s), [base, `claude-code:${sessions.c1}`])

      // CORS: no Access-Control-Allow-Origin, so the response is unreadable.
      expect(out.corsFetch.error).toBeTruthy()
      expect(out.corsHealth.error).toBeTruthy()
      // no-cors: opaque, no body.
      expect(out.noCors.body ?? null).toBeNull()
      // EventSource: refused, nothing delivered.
      expect(out.sse.got).toBeNull()
      // No existence oracle through subresource loads.
      expect(out.scriptExisting).toBe(out.scriptMissing)
      expect(out.imgExisting).toBe(out.imgMissing)

      // The daemon refused those requests itself, not just the browser
      // hiding the answers. From 127.0.0.1:<other port> the browser says
      // `same-site` and attaches the cookie; the auth layer refuses and
      // counts it. From localhost:<port> the request is `cross-site` and the
      // origin guard in front of it refuses it first (403, not counted).
      const after = (await (await fetch(`${base}/health`, { headers: authHeaders })).json()).auth.refused_requests as number
      if (host === '127.0.0.1') expect(after).toBeGreaterThan(before)
    } finally {
      await evil.close()
    }
    if (host === '127.0.0.1') {
      // Documented residual risk (docs/security.md): browsers send loopback
      // cookies to every port of the same host name.
      test.info().annotations.push({ type: 'observed', description: `attacker server received a TailFlow cookie: ${evil.cookies.some(c => c.includes('tailflow_session_'))}` })
    }
  })
}

test('the dashboard cannot be framed by another loopback page', async ({ page }) => {
  const { base } = state()
  const evil = await attackerServer(`<!doctype html><iframe id="f" src="${base}/#/sessions" width="800" height="600"></iframe>`)
  try {
    await page.goto(`http://127.0.0.1:${evil.port}/`)
    await page.waitForTimeout(1500)
    const frame = page.frames().find(f => f.url().startsWith(base))
    // Blocked frames never get the dashboard's document.
    const rendered = frame ? await frame.locator('.sidebar').count().catch(() => 0) : 0
    expect(rendered).toBe(0)
  } finally {
    await evil.close()
  }
})

test('without a session the dashboard shows sign-in and no data', async ({ browser }) => {
  const { base } = state()
  const context = await browser.newContext({ storageState: undefined })
  const page = await context.newPage()
  const responses: number[] = []
  page.on('response', r => { if (r.url().includes('/api/')) responses.push(r.status()) })
  await page.goto(`${base}/#/sessions`)
  await expect(page.getByRole('heading', { name: 'Sign in to this daemon' })).toBeVisible()
  await expect(page.getByText('tailflow-daemon dashboard --port')).toBeVisible()
  expect(await page.locator('.sidebar').count()).toBe(0)
  expect(responses.every(s => s === 401)).toBe(true)
  await context.close()
})

test('a sign-in link works once, and a refused link explains what to do', async ({ browser }) => {
  const d = await startDaemon('while true; do echo "INFO hi"; sleep 1; done\n')
  const context = await browser.newContext({ storageState: undefined })
  const page = await context.newPage()
  try {
    const r = await fetch(`${d.base}/api/v1/auth/login-links`, { method: 'POST', headers: authHeaders })
    const link = `${d.base}${(await r.json()).path}`
    await page.goto(link)
    expect(new URL(page.url()).pathname).toBe('/')
    expect(new URL(page.url()).search).toBe('')
    await expect(page.getByRole('navigation', { name: 'Main' })).toBeVisible()
    const cookie = (await context.cookies(d.base)).find(c => c.name === `tailflow_session_${d.port}`)
    expect(cookie?.httpOnly).toBe(true)
    expect(cookie?.sameSite).toBe('Strict')
    // Not readable from script.
    expect(await page.evaluate(() => document.cookie)).not.toContain('tailflow_session_')
    // Reusing the link (e.g. from history) does nothing.
    const reuse = await (await browser.newContext({ storageState: undefined })).newPage()
    const res = await reuse.goto(link)
    expect(res?.status()).toBe(401)
    await expect(reuse.getByText('expired or was already used')).toBeVisible()
    // Multiple tabs share the session.
    const second = await context.newPage()
    await second.goto(`${d.base}/#/logs`)
    await expect(second.getByRole('status', { name: 'Live log stream: live' })).toBeVisible()
    // Sign out from Settings ends it for every tab.
    await page.goto(`${d.base}/#/settings`)
    await page.getByRole('button', { name: 'Sign out of this daemon' }).click()
    await expect(page.getByRole('heading', { name: 'Sign in to this daemon' })).toBeVisible()
    await second.reload()
    await expect(second.getByRole('heading', { name: 'Sign in to this daemon' })).toBeVisible()
  } finally {
    await context.close()
    await d.stop()
  }
})

test('revoking sessions sends an open dashboard to sign-in', async ({ browser }) => {
  const d = await startDaemon('while true; do echo "INFO hi"; sleep 1; done\n')
  const context = await browser.newContext({ storageState: undefined })
  const page = await context.newPage()
  try {
    await d.signIn(page)
    await page.goto(`${d.base}/#/logs`)
    await expect(page.getByRole('status', { name: 'Live log stream: live' })).toBeVisible()
    const r = await fetch(`${d.base}/api/v1/auth/sessions/revoke`, { method: 'POST', headers: authHeaders })
    expect(r.status).toBe(200)
    // Any next request is refused; navigating shows sign-in.
    await page.goto(`${d.base}/#/sessions`)
    await page.reload()
    await expect(page.getByRole('heading', { name: 'Sign in to this daemon' })).toBeVisible()
  } finally {
    await context.close()
    await d.stop()
  }
})
