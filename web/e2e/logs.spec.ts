// Live log feed: virtualization bounds, live updates, pause/resume, JSON
// expansion, and the SSE lifecycle (reconnect, daemon restart, shutdown
// with the dashboard connected).

import { test, expect } from '@playwright/test'
import { startDaemon, type Daemon } from './daemon'

const BURST = `i=0
while [ $i -lt 6000 ]; do
  echo "INFO request $i ok"
  if [ $((i % 500)) -eq 0 ]; then echo '{"level":"error","msg":"json line","n":'$i',"nested":{"a":[1,2,3]}}'; fi
  i=$((i+1))
done
while true; do echo "INFO tick"; sleep 1; done
`

test.describe('live log feed', () => {
  let d: Daemon
  test.beforeAll(async () => { d = await startDaemon(BURST) })
  test.afterAll(async () => { await d.stop() })

  test('renders 5,000 lines with a bounded DOM', async ({ page }) => {
    await page.goto(`${d.base}/#/logs`)
    await expect(page.getByText(/5,000 live lines shown|of 5,000 live lines/)).toBeVisible({ timeout: 15_000 })
    const rows = await page.locator('.log-row').count()
    expect(rows).toBeGreaterThan(10)
    expect(rows).toBeLessThan(150)
    const nodes = await page.evaluate(() => document.getElementsByTagName('*').length)
    expect(nodes).toBeLessThan(2_500)
    // Following: the newest line is in view.
    await expect(page.locator('.log-row').last()).toContainText('tick')
  })

  test('filters, expands JSON, pauses without moving, and resumes', async ({ page }) => {
    await page.goto(`${d.base}/#/logs`)
    await expect(page.getByText(/of 5,000 live lines/)).toBeVisible({ timeout: 15_000 })
    await page.getByLabel('Filter live log lines').fill('json line')
    // One JSON line per 500 in the burst; the ring keeps the newest 5,000.
    await expect.poll(() => page.locator('.log-row').count()).toBeGreaterThanOrEqual(8)
    for (const t of await page.locator('.log-row').allTextContents()) expect(t).toContain('json line')
    await page.locator('.log-row').first().getByRole('button', { name: 'Expand JSON' }).click()
    await expect(page.locator('.json-block').first()).toContainText('"nested"')
    await page.getByLabel('Filter live log lines').fill('')

    await page.getByRole('button', { name: 'Pause' }).click()
    const feed = page.locator('.feed .vlist')
    await feed.evaluate(el => { el.scrollTop = el.scrollHeight / 2 })
    await page.waitForTimeout(300)
    const anchor = await page.locator('.log-row').nth(5).textContent()
    const top = await feed.evaluate(el => el.scrollTop)
    await page.waitForTimeout(2_500) // new ticks arrive while paused
    expect(await feed.evaluate(el => el.scrollTop)).toBe(top)
    expect(await page.locator('.log-row').nth(5).textContent()).toBe(anchor)
    await expect(page.getByRole('button', { name: /new line|Resume live/ })).toBeVisible()
    await page.getByRole('button', { name: /new line|Resume live/ }).click()
    await expect(page.locator('.log-row').last()).toContainText('tick')
  })
})

test('reconnects after a daemon restart and resets to the new process', async ({ page }) => {
  let d = await startDaemon('while true; do echo "INFO alive $(date +%s)"; sleep 0.5; done\n')
  try {
    await page.goto(`${d.base}/#/logs`)
    await expect(page.getByRole('status', { name: 'Live log stream: live' })).toBeVisible()
    await expect(page.locator('.log-row').first()).toBeVisible()

    // Shutdown with the dashboard's SSE stream open must not hang (M1.5-F).
    const { code, ms } = await d.stop('SIGTERM')
    expect(code).toBe(0)
    expect(ms).toBeLessThan(5_000)
    await expect(page.getByRole('status', { name: /Live log stream: (reconnecting|disconnected)/ })).toBeVisible()

    d = await d.restart()
    await expect(page.getByRole('status', { name: 'Live log stream: live' })).toBeVisible({ timeout: 15_000 })
    await expect(page.getByText(/daemon restarted once/)).toBeVisible()
    await expect(page.locator('.log-row').first()).toContainText('alive')
  } finally {
    await d.stop()
  }
})

test('opening the view under continuous load hydrates the whole ring at once', async ({ page }) => {
  // Regression: live records arriving before the first catch-up used to set
  // its cursor, so the ring was skipped and the view filled at the line rate.
  const d = await startDaemon(`i=0
while [ $i -lt 3000 ]; do echo "INFO backlog $i"; i=$((i+1)); done
while true; do echo "INFO live"; sleep 0.02; done
`)
  try {
    await expect.poll(async () => (await (await fetch(`${d.base}/health`)).json()).buffered).toBeGreaterThan(3000)
    await page.goto(`${d.base}/#/logs`)
    await expect(page.getByText(/of [3-5],\d{3} live lines/)).toBeVisible({ timeout: 4_000 })
    await page.getByLabel('Filter live log lines').fill('backlog 0')
    await expect(page.locator('.log-row').first()).toContainText('backlog 0')
  } finally {
    await d.stop()
  }
})

test('a fast stream keeps following; only the user leaves the tail', async ({ page }) => {
  // Regression: our own scroll-to-bottom, read after newer rows grew the
  // content, used to look like the user scrolling away and froze the view.
  // ~1,000 lines/s: 50 lines every 50 ms.
  const d = await startDaemon(`while true; do
  i=0; while [ $i -lt 50 ]; do echo "INFO fast line $i"; i=$((i+1)); done
  sleep 0.05
done
`)
  try {
    await page.goto(`${d.base}/#/logs`)
    await expect(page.locator('.log-row').first()).toBeVisible()
    for (let i = 0; i < 4; i++) {
      await page.waitForTimeout(1000)
      await expect(page.locator('.feed-pill')).toHaveCount(0)
    }
    await page.locator('.feed .vlist').hover()
    await page.mouse.wheel(0, -600)
    await expect(page.locator('.feed-pill')).toBeVisible()
  } finally {
    await d.stop()
  }
})
