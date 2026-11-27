// M4-I: keyboard access to timeline items, and the M4 views at a phone
// width (no horizontal page scroll, content reachable).

import { test, expect, type Page } from '@playwright/test'

test('timeline items are reachable and opened from the keyboard', async ({ page }) => {
  await page.goto('/#/timeline')
  const tl = page.getByRole('group', { name: /^Timeline from/ })
  await expect(tl).toBeVisible()
  await expect(tl).toHaveAttribute('aria-label', /up and down arrows to step through items/)
  await tl.focus()
  await page.keyboard.press('ArrowDown')
  const live = tl.locator('[aria-live="polite"]')
  await expect(live).toHaveText(/^1 of \d+: /)
  await expect(page).toHaveURL(/event=\d+/)
  await expect(page.getByRole('complementary', { name: 'Event details' })).toBeVisible()
  const first = new URL(page.url().replace('#/', '')).searchParams.get('event')
  // End jumps to the last item; Up steps back; Home returns to the first.
  await tl.focus()
  await page.keyboard.press('End')
  await expect(live).toHaveText(/^(\d+) of \1: /)
  await page.keyboard.press('ArrowUp')
  await page.keyboard.press('Home')
  await expect(live).toHaveText(/^1 of \d+: /)
  await expect.poll(() => new URL(page.url().replace('#/', '')).searchParams.get('event')).toBe(first)
})

async function noHorizontalScroll(page: Page, label: string) {
  await page.waitForTimeout(500)
  const { scroll, client } = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth,
  }))
  expect(scroll, `${label}: page scrolls sideways (${scroll} > ${client})`).toBeLessThanOrEqual(client + 1)
  await expect(page.locator('h1')).toBeVisible()
}

test.describe('M4 views at 390 px', () => {
  test.use({ viewport: { width: 390, height: 844 } })

  test('Files, Commands, Session Git changes, Integrations and the Inspector fit', async ({ page }) => {
    await page.goto('/#/files')
    const verified = page.getByRole('tab', { name: /verified/i }).or(page.getByRole('button', { name: /verified/i })).first()
    if (await verified.count()) await verified.click()
    await noHorizontalScroll(page, 'files/verified')

    await page.goto('/#/commands')
    const outcomes = page.getByRole('tab', { name: /tests, builds/i }).or(page.getByRole('button', { name: /tests, builds/i })).first()
    if (await outcomes.count()) await outcomes.click()
    await noHorizontalScroll(page, 'commands/outcomes')

    await page.goto('/#/integrations')
    await noHorizontalScroll(page, 'integrations')

    await page.goto('/#/sessions')
    await page.locator('tbody tr').first().click()
    await expect(page).toHaveURL(/#\/sessions\/.+/)
    const git = page.getByRole('tab', { name: /git changes/i }).or(page.getByRole('button', { name: /git changes/i })).first()
    if (await git.count()) await git.click()
    await noHorizontalScroll(page, 'session git changes')

    await page.goto('/#/activity')
    await page.locator('.act-row').first().click()
    const inspector = page.getByRole('complementary', { name: 'Event details' })
    await expect(inspector).toBeVisible()
    await expect(inspector.getByText(/Related evidence/i).first()).toBeVisible()
    await noHorizontalScroll(page, 'inspector')
  })
})
