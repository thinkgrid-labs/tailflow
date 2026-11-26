// Command outcomes (M4-E) and evidence-graded correlation (M4-F) against
// the shared fixture: Claude `cargo test` (exit 101, then passing),
// `npm run build`, `npm test` (exit 1); Codex `npm run lint` (no outcome
// reported) and `make migrate` (exit 2).

import { test, expect } from '@playwright/test'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

test('commands are classified, with outcomes only as reported', async ({ page }) => {
  await page.goto('/#/commands?view=outcomes')
  const table = page.locator('table')
  await expect(page.getByText(/only as the agent reported it/)).toBeVisible()
  const row = (re: RegExp) => table.getByRole('row').filter({ hasText: re })
  await expect(row(/cargo test/).filter({ hasText: 'exit 101' })).toContainText('Test')
  await expect(row(/cargo test/).filter({ hasText: 'exit 101' })).toContainText('Reported failure')
  await expect(row(/npm run build/).first()).toContainText('Build')
  await expect(row(/npm run lint/).first()).toContainText('Lint')
  await expect(row(/npm run lint/).first()).toContainText('Outcome unknown')
  // Never presented as verified.
  await expect(page.locator('table').getByText('Verified exit', { exact: true })).toHaveCount(0)
  // Per-kind counts link to a filtered view.
  await page.locator('a.stat', { hasText: 'Test' }).click()
  await expect(page).toHaveURL(/category=test/)
  await expect(table.getByRole('row').filter({ hasText: /npm run build/ })).toHaveCount(0)
})

test('related evidence is graded by tier and never causal', async ({ page }) => {
  await page.goto('/#/commands?view=outcomes')
  await page.locator('table').getByRole('row').filter({ hasText: 'exit 101' }).first().click()
  const insp = page.getByRole('complementary', { name: 'Event details' })
  await expect(insp.getByRole('heading', { name: 'Related evidence' })).toBeVisible()
  await expect(insp.getByText('1. Direct identifiers')).toBeVisible()
  await expect(insp.getByText('4. Temporal proximity')).toBeVisible()
  await expect(insp.getByText(/does not observe processes/)).toBeVisible()
  await expect(insp.getByText(/None of them is a claim of cause and effect/)).toBeVisible()
  const text = (await insp.textContent()) ?? ''
  expect(text).not.toMatch(/\bcaused\b|because of|due to/i)
})

test('a session lists its tests, builds and checks', async ({ page }) => {
  const state = JSON.parse(fs.readFileSync(path.join(os.tmpdir(), 'tailflow-e2e-state.json'), 'utf8'))
  await page.goto(`/#/sessions/${encodeURIComponent('claude-code:' + state.sessions.c1)}?tab=summary`)
  const card = page.locator('.card', { has: page.getByRole('heading', { name: 'Tests, builds & checks' }) })
  await expect(card.getByRole('row').filter({ hasText: 'exit 101' })).toContainText('Test')
})
