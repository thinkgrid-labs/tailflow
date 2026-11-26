// Dashboard behaviour against the real daemon and the fixture dataset
// (5 sessions: Claude Code C1 ended/complete, C2 active, C3 joined mid-way;
// Codex X1 ended, X2 never ended; one rejected hook input = recorded loss).

import { test, expect, type Page } from '@playwright/test'
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'

const state = () => JSON.parse(fs.readFileSync(path.join(os.tmpdir(), 'tailflow-e2e-state.json'), 'utf8')) as {
  sessions: Record<'c1' | 'c2' | 'c3' | 'x1' | 'x2', string>
}
const sid = (k: 'c1' | 'c2' | 'c3' | 'x1' | 'x2') => (k.startsWith('c') ? 'claude-code:' : 'codex:') + state().sessions[k]

function noErrors(page: Page) {
  const errors: string[] = []
  page.on('pageerror', e => errors.push(e.message))
  page.on('console', m => { if (m.type() === 'error') errors.push(m.text()) })
  return errors
}

const stat = (page: Page, label: string) => page.locator('.stat', { has: page.locator('.label', { hasText: new RegExp(`^${label}$`) }) }).locator('.value')

test('overview counts come from the aggregate endpoint and match the dataset', async ({ page }) => {
  const errors = noErrors(page)
  await page.goto('/#/overview')
  await expect(stat(page, 'Active sessions')).toHaveText('1')
  await expect(stat(page, 'Sessions')).toHaveText('5')
  await expect(stat(page, 'Tool calls')).toHaveText('17')
  await expect(stat(page, 'Commands')).toHaveText('6')
  // Two failing Bash calls (each reported as a tool and a command failure)
  // and one Codex command failure: three failed operations, not five events.
  await expect(stat(page, 'Failed operations')).toHaveText('3')
  await expect(stat(page, 'Reported file changes')).toHaveText('7')
  // Loss is disclosed, and installation is not mistaken for activity.
  await expect(page.getByText(/recorded loss gap overlaps this range/)).toBeVisible()
  await expect(page.locator('.card', { hasText: 'Claude Code' }).getByText('Not installed').first()).toBeVisible()
  await expect(page.getByText('Recent sessions')).toBeVisible()
  expect(errors).toEqual([])
})

test('session list filters by agent, status and text, and paginates', async ({ page }) => {
  await page.goto('/#/sessions')
  await expect(page.locator('tbody tr')).toHaveCount(5)
  await page.getByLabel('Agent').selectOption('codex')
  await expect(page.locator('tbody tr')).toHaveCount(2)
  await expect(page).toHaveURL(/agent=codex/)
  await page.getByLabel('Agent').selectOption('')
  await page.getByLabel('Status').selectOption('unterminated')
  await expect(page.locator('tbody tr')).toHaveCount(2)
  await expect(page.locator('tbody').getByText('No end observed')).toHaveCount(2)
  await page.getByLabel('Status').selectOption('')
  await page.getByLabel('Search sessions').fill('web-app')
  await page.getByRole('button', { name: 'Search' }).click()
  await expect(page.locator('tbody tr')).toHaveCount(2)
})

test('session detail shows lifecycle, git boundaries and coverage honestly', async ({ page }) => {
  await page.goto(`/#/sessions/${encodeURIComponent(sid('c1'))}`)
  await expect(page.getByText('History complete').first()).toBeVisible()
  await expect(page.getByText(/HEAD differs between the observed start and end/)).toBeVisible()
  await expect(page.getByText(/not proof the agent made the commit/)).toBeVisible()
  await expect(page.getByRole('list', { name: 'Session events' })).toBeVisible()
  await page.getByRole('button', { name: 'Coverage' }).click()
  await expect(page.getByText('Integration limits')).toBeVisible()
  await expect(page.getByText(/means "not seen", not "did not happen"/)).toBeVisible()

  // A session joined mid-way is never presented as complete.
  await page.goto(`/#/sessions/${encodeURIComponent(sid('c3'))}`)
  await expect(page.getByText('History unknown').first()).toBeVisible()
  await expect(page.getByText(/Session start was not observed/).first()).toBeVisible()
  await expect(page.getByText('No end observed').first()).toBeVisible()
})

test('activity search uses the indexed backend, opens the inspector and keeps the URL', async ({ page }) => {
  await page.goto('/#/activity')
  const box = page.getByLabel('Search history')
  await box.fill('program:cargo')
  await box.press('Enter')
  await expect(page).toHaveURL(/program=cargo/)
  await expect(page.locator('.act-row').first()).toContainText('cargo')
  const rows = await page.locator('.act-row .what').allTextContents()
  expect(rows.length).toBeGreaterThan(0)
  expect(rows.every(t => /cargo/.test(t))).toBe(true)
  await expect(page.locator('.chip', { hasText: 'program: cargo' })).toBeVisible()

  await page.locator('.act-row', { hasText: 'exited 101' }).first().click()
  const insp = page.getByRole('complementary', { name: 'Event details' })
  await expect(insp).toBeVisible()
  await expect(insp.getByText('Agent-reported')).toBeVisible()
  await expect(insp.getByRole('heading', { name: 'Operation' })).toBeVisible()
  await expect(page).toHaveURL(/event=\d+/)
  await page.keyboard.press('Escape')
  await expect(insp).toBeHidden()

  // Suggestions for supported filters.
  await box.fill('ag')
  await expect(page.getByRole('option', { name: /agent:/ })).toBeVisible()
})

test('file events are worded as reported, never verified', async ({ page }) => {
  await page.goto('/#/activity?type=files&since=7d')
  await page.locator('.act-row', { hasText: 'Edited' }).first().click()
  const insp = page.getByRole('complementary', { name: 'Event details' })
  await expect(insp.getByText('Reported modified')).toBeVisible()
  await expect(insp.getByText(/not independently verified/)).toBeVisible()

  await page.goto('/#/files')
  await expect(page.getByText(/These are claims/)).toBeVisible()
  await page.getByText('Touched by more than one agent').click()
  await expect(page.locator('tbody tr')).toHaveCount(2)
  await expect(page.locator('tbody').getByText('shared')).toHaveCount(2)
})

test('commands never claim an outcome that was not reported', async ({ page }) => {
  await page.goto('/#/commands')
  await expect(page.locator('tbody tr')).toHaveCount(6)
  const codexNpm = page.locator('tbody tr', { hasText: 'Codex' }).filter({ hasText: 'npm' })
  await expect(codexNpm.getByText('Outcome not reported')).toBeVisible()
  await expect(page.locator('tbody tr', { hasText: 'cargo' }).getByText('Exit 101')).toBeVisible()
  await expect(page.locator('tbody').getByText('Exit 0')).toHaveCount(0)
})

test('errors view lists reported failures and runtime errors', async ({ page }) => {
  await page.goto('/#/errors')
  await expect(page.locator('.act-row', { hasText: 'connection refused' }).first()).toBeVisible()
  // The fixture's api source logs an error every 10 s, so on a slow run the
  // older agent failures sit below the virtualized window: scroll to them.
  const list = page.getByRole('list', { name: 'Errors' })
  for (const text of ['exited 101', 'make exited 2']) {
    const row = page.locator('.act-row', { hasText: text })
    await list.evaluate(el => { el.scrollTop = 0 })
    for (let i = 0; i < 40 && !(await row.count()); i++) {
      await list.evaluate(el => { el.scrollTop += el.clientHeight / 2 })
      await page.waitForTimeout(50)
    }
    await row.first().scrollIntoViewIfNeeded()
    await expect(row.first()).toBeVisible()
  }
})

test('timeline draws agent lanes, intervals and runtime markers; zoom updates the URL', async ({ page }) => {
  const now = Date.now()
  await page.goto(`/#/timeline?from=${now - 105 * 60e3}&to=${now + 60e3}`)
  const tl = page.locator('.tl')
  await expect(tl.getByText('Claude Code', { exact: true })).toBeVisible()
  await expect(tl.getByText('Codex', { exact: true })).toBeVisible()
  await expect(tl.getByText('Runtime', { exact: true })).toBeVisible()
  await expect(tl.locator('rect.item').first()).toBeVisible()
  expect(await tl.locator('.item').count()).toBeGreaterThan(20)
  // Codex tool calls report no duration and no start/end pair for shell
  // outcomes: those are points (paths), not bars.
  expect(await tl.locator('path.item').count()).toBeGreaterThan(5)
  await tl.locator('rect.item.k-failure').first().click()
  await expect(page.getByRole('complementary', { name: 'Event details' })).toBeVisible()
  const before = new URL(page.url().replace('#/', '')).searchParams.get('from')
  await page.getByRole('button', { name: 'Zoom in' }).click()
  await expect.poll(() => new URL(page.url().replace('#/', '')).searchParams.get('from')).not.toBe(before)
})

test('integrations page is read-only and explains coverage', async ({ page }) => {
  await page.goto('/#/integrations')
  await expect(page.locator('.cmd-copy', { hasText: 'tailflow agent install claude' })).toBeVisible()
  await expect(page.getByText('Not observable').first()).toBeVisible()
  await expect(page.getByText('Recorded gaps')).toBeVisible()
  await expect(page.locator('button', { hasText: /^Install/ })).toHaveCount(0)
})

test('theme and time zone settings apply and persist', async ({ page }) => {
  await page.goto('/#/settings')
  await page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'Light' }).click()
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  await page.getByRole('group', { name: 'Time zone' }).getByRole('button', { name: 'UTC' }).click()
  await expect(page.getByText(/^Now: .* UTC$/)).toBeVisible()
  await page.reload()
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light')
  const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor)
  expect(bg).toBe('rgb(246, 247, 249)')
  await page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'System' }).click()
  await expect(page.locator('html')).not.toHaveAttribute('data-theme', /.+/)
})

test('mobile layout uses a navigation drawer and does not overflow', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  for (const hash of ['#/overview', '#/sessions', '#/activity', '#/timeline', '#/logs', '#/files', '#/integrations']) {
    await page.goto(`/${hash}`)
    await page.waitForTimeout(400)
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)
    expect(overflow, hash).toBeLessThanOrEqual(1)
  }
  await page.goto('/#/overview')
  const menu = page.getByRole('button', { name: 'Open navigation' })
  await expect(menu).toBeVisible()
  await menu.click()
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Sessions' }).click()
  await expect(page).toHaveURL(/#\/sessions/)
  await expect(menu).toHaveAttribute('aria-expanded', 'false')
})
