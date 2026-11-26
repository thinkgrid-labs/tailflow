// Automated accessibility audit (axe-core) of every view in both themes,
// plus the M4 views in their interactive states. Any WCAG 2.2 A/AA
// violation fails the test with a readable summary.

import { test, expect, type Page } from '@playwright/test'
import AxeBuilder from '@axe-core/playwright'

const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa']

const PAGES = [
  '#/overview', '#/sessions', '#/activity', '#/errors', '#/timeline', '#/logs',
  '#/files', '#/commands', '#/integrations', '#/settings',
]

async function audit(page: Page, label: string) {
  // Let loads settle and transitions finish before auditing (the agent
  // stream keeps a connection open, so "network idle" never comes).
  await expect(page.locator('.skeleton')).toHaveCount(0, { timeout: 8_000 }).catch(() => {})
  await page.waitForTimeout(400)
  const r = await new AxeBuilder({ page }).withTags(TAGS).analyze()
  const summary = r.violations.map(v =>
    `${v.id} (${v.impact}): ${v.help}\n` +
    v.nodes.slice(0, 5).map(n => `    ${n.target.join(' ')} — ${n.failureSummary?.split('\n')[1]?.trim() ?? ''}`).join('\n'))
  expect.soft(summary, `${label}\n${summary.join("\n")}`).toEqual([])
}

for (const scheme of ['light', 'dark'] as const) {
  test.describe(`axe (${scheme})`, () => {
    test.use({ colorScheme: scheme })
    // Ten audits per test; WebKit's axe runs are several times slower.
    test.setTimeout(120_000)

    test('every view', async ({ page }) => {
      for (const hash of PAGES) {
        await page.goto(`/${hash}`)
        await expect(page.locator('h1')).toHaveCount(1)
        await audit(page, `${scheme} ${hash}`)
      }
    })

    test('M4 views in their interactive states', async ({ page }) => {
      // Files → Verified on disk.
      await page.goto('/#/files')
      const verified = page.getByRole('tab', { name: /verified/i }).or(page.getByRole('button', { name: /verified/i })).first()
      if (await verified.count()) await verified.click()
      await audit(page, `${scheme} files/verified`)

      // Commands → Tests, builds & checks.
      await page.goto('/#/commands')
      const outcomes = page.getByRole('tab', { name: /tests, builds/i }).or(page.getByRole('button', { name: /tests, builds/i })).first()
      if (await outcomes.count()) await outcomes.click()
      await audit(page, `${scheme} commands/outcomes`)

      // Session details → Git changes.
      await page.goto('/#/sessions')
      await page.locator('tbody tr').first().click()
      await expect(page).toHaveURL(/#\/sessions\/.+/)
      await audit(page, `${scheme} session detail`)
      const git = page.getByRole('tab', { name: /git changes/i }).or(page.getByRole('button', { name: /git changes/i })).first()
      if (await git.count()) {
        await git.click()
        await audit(page, `${scheme} session git changes`)
      }

      // Activity → Inspector with related evidence.
      await page.goto('/#/activity')
      const row = page.locator('[role="row"], tbody tr, [data-seq]').first()
      await row.click()
      await audit(page, `${scheme} inspector`)
    })
  })
}

test('the sign-in screen', async ({ browser, baseURL }) => {
  // No session cookie: the dashboard shows sign-in.
  const ctx = await browser.newContext({ baseURL, storageState: { cookies: [], origins: [] } })
  const page = await ctx.newPage()
  await page.goto('/#/overview')
  await expect(page.getByRole('heading', { level: 1 })).toBeVisible()
  await audit(page, 'sign-in')
  await ctx.close()
})
