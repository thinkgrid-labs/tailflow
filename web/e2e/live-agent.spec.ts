// Live agent activity (M4-B): committed agent events reach the open
// dashboard through the stream, without periodic polling, and the stream
// recovers across a daemon restart.

import { test, expect } from '@playwright/test'
import { startDaemon, type Daemon } from './daemon'

const IDLE = 'while true; do sleep 5; done\n'

/** A completed Read tool call (counts as one tool call). */
function tool(cwd: string, sid: string, id: string, file: string) {
  return { session_id: sid, cwd, hook_event_name: 'PostToolUse', tool_name: 'Read', tool_use_id: id, tool_input: { file_path: `${cwd}/${file}` }, tool_response: { type: 'text' } }
}

test.describe('live agent stream', () => {
  let d: Daemon
  test.beforeEach(async () => { d = await startDaemon(IDLE, { agents: true }) })
  test.afterEach(async () => { await d.stop() })

  test('a new session appears within seconds, without polling', async ({ page }) => {
    await d.signIn(page)
    const requests: { url: string; at: number }[] = []
    page.on('request', r => { if (r.url().includes('/api/v1/sessions')) requests.push({ url: r.url(), at: Date.now() }) })
    await page.goto(`${d.base}/#/sessions`)
    await expect(page.getByRole('status', { name: 'Agent event stream: live' })).toBeVisible()
    await expect(page.getByText(/No sessions|no agent sessions/i)).toBeVisible()

    // The stream's first `ready` reloads each live view once (closing the
    // window between the page's first load and the subscription); after
    // that, a quiet period: no periodic session polling while live.
    await page.waitForTimeout(1_500)
    const before = requests.length
    await page.waitForTimeout(16_000)
    expect(requests.length - before, 'no 15 s poll while live').toBe(0)

    const t0 = Date.now()
    d.hook({ session_id: 'live-1', cwd: d.dir, hook_event_name: 'SessionStart', source: 'startup' })
    await expect(page.getByRole('row', { name: /live-1/ })).toBeVisible({ timeout: 5_000 })
    const latency = Date.now() - t0
    test.info().annotations.push({ type: 'hook→display ms', description: String(latency) })
    console.log(`hook→display: ${latency} ms`)
    expect(latency).toBeLessThan(5_000)
  })

  test('session detail updates live and coalesces bursts', async ({ page }) => {
    d.hook({ session_id: 'burst', cwd: d.dir, hook_event_name: 'SessionStart', source: 'startup' })
    await expect.poll(async () => (await d.api('/api/v1/sessions')).sessions?.length ?? 0).toBe(1)
    const key = (await d.api('/api/v1/sessions')).sessions[0].session_id as string
    await d.signIn(page)
    let detailLoads = 0
    page.on('request', r => { if (r.url().includes('/api/v1/sessions/')) detailLoads++ })
    await page.goto(`${d.base}/#/sessions/${encodeURIComponent(key)}`)
    await expect(page.getByRole('status', { name: 'Agent event stream: live' })).toBeVisible()
    // The first detail load has rendered before the burst starts.
    const toolStat = page.locator('.stat', { hasText: 'Tool calls' })
    await expect(toolStat.locator('.value')).toHaveText('0')
    await page.waitForTimeout(1_500)
    const start = detailLoads
    // 40 tool calls in a burst: the page reloads a few times, not 40.
    for (let i = 0; i < 40; i++) d.hook(tool(d.dir, 'burst', `b${i}`, `f${i}.rs`))
    await expect.poll(async () => (await d.api(`/api/v1/sessions/${encodeURIComponent(key)}`)).counts?.tool_calls ?? 0, { timeout: 10_000 }).toBeGreaterThanOrEqual(40)
    await expect(toolStat.locator('.value')).toHaveText('40', { timeout: 5_000 })
    const reloads = detailLoads - start
    test.info().annotations.push({ type: 'detail reloads for 40 events', description: String(reloads) })
    console.log(`detail reloads for 40 events: ${reloads}`)
    expect(reloads).toBeGreaterThanOrEqual(1)
    expect(reloads).toBeLessThanOrEqual(12)
  })

  test('the stream reconnects after a daemon restart and catches up', async ({ page }) => {
    await d.signIn(page)
    await page.goto(`${d.base}/#/sessions`)
    await expect(page.getByRole('status', { name: 'Agent event stream: live' })).toBeVisible()
    await d.stop()
    await expect(page.getByRole('status', { name: /Agent event stream: (reconnecting|polling)/ })).toBeVisible()
    // Committed by the restarted daemon from the spool, while this tab was
    // disconnected.
    d.hook({ session_id: 'while-down', cwd: d.dir, hook_event_name: 'SessionStart', source: 'startup' })
    d = await d.restart()
    await expect(page.getByRole('status', { name: 'Agent event stream: live' })).toBeVisible({ timeout: 15_000 })
    await expect(page.getByRole('row', { name: /while-do/ })).toBeVisible({ timeout: 10_000 })
  })
})
