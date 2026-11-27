// Accessibility checks that do not need an extra dependency: landmarks,
// names on every control, keyboard reachability, focus visibility, status
// not conveyed by color alone, reduced motion.

import { test, expect } from '@playwright/test'

const PAGES = ['#/overview', '#/sessions', '#/activity', '#/errors', '#/timeline', '#/logs', '#/files', '#/commands', '#/integrations', '#/settings']

test('every interactive element has an accessible name', async ({ page }) => {
  for (const hash of PAGES) {
    await page.goto(`/${hash}`)
    await page.waitForTimeout(700)
    const unnamed = await page.evaluate(() => {
      const out: string[] = []
      for (const el of document.querySelectorAll<HTMLElement>('button, a[href], input, select, [role="button"]')) {
        if (el.closest('[aria-hidden="true"]') || el.offsetParent === null) continue
        const labelled = el.getAttribute('aria-label') || el.getAttribute('aria-labelledby') || el.getAttribute('title')
          || (el as HTMLInputElement).labels?.length || (el.textContent ?? '').trim() || el.getAttribute('placeholder')
        if (!labelled) out.push(el.outerHTML.slice(0, 120))
      }
      return out
    })
    expect(unnamed, hash).toEqual([])
  }
})

test('landmarks and headings exist on every page', async ({ page }) => {
  for (const hash of PAGES) {
    await page.goto(`/${hash}`)
    await expect(page.getByRole('navigation', { name: 'Main' })).toBeVisible()
    await expect(page.getByRole('main')).toBeVisible()
    await expect(page.locator('h1')).toHaveCount(1)
    await expect(page.locator('main h2').first()).toBeVisible()
  }
})

test('keyboard: skip link, search shortcut, focus ring, row activation', async ({ page, browserName }) => {
  await page.goto('/#/sessions')
  // Tab only once the app has rendered (otherwise focus lands elsewhere).
  await expect(page.locator('tbody tr').first()).toBeVisible()
  // macOS WebKit (Safari's default) moves Tab focus to links only with
  // Option held; Alt+Tab is the keyboard path its users have.
  await page.keyboard.press(browserName === 'webkit' && process.platform === 'darwin' ? 'Alt+Tab' : 'Tab')
  await expect(page.getByText('Skip to content')).toBeFocused()
  await page.locator('main').click({ position: { x: 5, y: 5 } })
  await page.keyboard.press('/')
  await expect(page.getByRole('searchbox', { name: 'Search all history' })).toBeFocused()
  const outline = await page.getByRole('searchbox', { name: 'Search all history' }).evaluate(el => getComputedStyle(el).borderColor)
  expect(outline).not.toBe('')
  await page.locator('tbody tr').first().focus()
  // The highlight fades in (120 ms transition): poll the computed color.
  await expect.poll(() => page.locator('tbody tr').first().evaluate(el => getComputedStyle(el).backgroundColor))
    .not.toBe('rgba(0, 0, 0, 0)')
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/#\/sessions\/.+/)
})

test('status badges carry text, not only color', async ({ page }) => {
  await page.goto('/#/sessions')
  const badges = page.locator('tbody .badge')
  await expect(badges.first()).toBeVisible()
  for (const t of await badges.allTextContents()) expect(t.trim().length).toBeGreaterThan(1)
})

test('reduced motion disables animation', async ({ browser, baseURL }) => {
  const ctx = await browser.newContext({ reducedMotion: 'reduce', baseURL })
  const page = await ctx.newPage()
  await page.goto('/#/overview')
  const anim = await page.evaluate(() => {
    const el = document.createElement('div')
    el.className = 'skeleton'
    document.body.appendChild(el)
    return getComputedStyle(el).animationName
  })
  expect(anim).toBe('none')
  await ctx.close()
})
