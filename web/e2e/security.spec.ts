// Untrusted text stays text; the page loads nothing from anywhere else.

import { test, expect } from '@playwright/test'

test('log and event text cannot inject markup or script', async ({ page }) => {
  const dialogs: string[] = []
  page.on('dialog', d => { dialogs.push(d.message()); void d.dismiss() })
  for (const hash of ['#/logs', '#/activity?type=runtime&q=untrusted', '#/errors']) {
    await page.goto(`/${hash}`)
    await page.waitForTimeout(1200)
  }
  // The feed is virtualized: filter so the hostile line is rendered.
  await page.goto('/#/logs')
  await page.getByLabel('Filter live log lines').fill('untrusted')
  await expect(page.getByText(/untrusted <img src=x/)).toBeVisible()
  expect(await page.evaluate(() => (window as unknown as { __xss?: number }).__xss)).toBeUndefined()
  expect(await page.locator('.feed img, .feed script').count()).toBe(0)
  expect(dialogs).toEqual([])
})

test('the UI is served with a strict CSP and makes same-origin requests only', async ({ page, baseURL }) => {
  const origins = new Set<string>()
  page.on('request', r => origins.add(new URL(r.url()).origin))
  const violations: string[] = []
  page.on('console', m => { if (/Content Security Policy/i.test(m.text())) violations.push(m.text()) })
  const res = await page.goto('/#/overview')
  const csp = res?.headers()['content-security-policy'] ?? ''
  expect(csp).toContain("script-src 'self'")
  expect(csp).toContain("frame-ancestors 'none'")
  expect(res?.headers()['x-content-type-options']).toBe('nosniff')
  for (const hash of ['#/sessions', '#/timeline', '#/integrations', '#/settings']) {
    await page.goto(`/${hash}`)
    await page.waitForTimeout(600)
  }
  expect([...origins]).toEqual([new URL(baseURL!).origin])
  expect(violations).toEqual([])
})

test('query parameters from the URL are validated by the daemon, not trusted', async ({ page }) => {
  await page.goto('/#/activity?status=bogus')
  await expect(page.getByText(/invalid `status`/)).toBeVisible()
  await page.goto('/#/sessions?status=%3Cscript%3E')
  await expect(page.getByText(/unknown status/)).toBeVisible()
  expect(await page.locator('script:not([src])').count()).toBe(0)
})
