// Verified file changes and Git evidence (M4-C/D) in the dashboard, against
// a real daemon observing a real Git repository.

import { test, expect } from '@playwright/test'
import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { startDaemon, type Daemon } from './daemon'

function git(dir: string, ...args: string[]) {
  execFileSync('git', args, { cwd: dir, env: { ...process.env, GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@example.invalid', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@example.invalid' } })
}

test.describe('file and Git evidence', () => {
  let d: Daemon
  let repo: string
  test.beforeAll(async () => {
    const base = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), 'tf-evidence-'))
    repo = path.join(base, 'repo')
    fs.mkdirSync(repo)
    git(repo, 'init', '-q', '-b', 'main')
    for (const f of ['agent.rs', 'claimed.rs', 'person.rs', 'old.rs']) fs.writeFileSync(path.join(repo, f), 'v1\n')
    git(repo, 'add', '.')
    git(repo, 'commit', '-q', '-m', 'init')
    fs.writeFileSync(path.join(repo, 'old.rs'), 'dirty before the session\n')
    d = await startDaemon('while true; do sleep 5; done\n', { agents: true, observe: repo })
    await expect.poll(async () => (await d.api('/api/v1/observe/status')).repos?.[0]?.snapshots ?? 0, { timeout: 20_000 }).toBeGreaterThan(0)
    const s = { session_id: 'ev-1', cwd: repo }
    d.hook({ ...s, hook_event_name: 'SessionStart', source: 'startup' })
    await expect.poll(async () => (await d.api('/api/v1/events?type=git.snapshot&session=claude-code:ev-1')).events?.length ?? 0, { timeout: 20_000 }).toBeGreaterThan(0)
    await new Promise(r => setTimeout(r, 500))
    fs.writeFileSync(path.join(repo, 'agent.rs'), 'v2\n')
    d.hook({ ...s, hook_event_name: 'PostToolUse', tool_name: 'Edit', tool_use_id: 'e1', tool_input: { file_path: path.join(repo, 'agent.rs') }, tool_response: {} })
    d.hook({ ...s, hook_event_name: 'PostToolUse', tool_name: 'Edit', tool_use_id: 'e2', tool_input: { file_path: path.join(repo, 'claimed.rs') }, tool_response: {} })
    fs.writeFileSync(path.join(repo, 'person.rs'), 'v2 by a person\n')
    await new Promise(r => setTimeout(r, 1000))
    d.hook({ ...s, hook_event_name: 'SessionEnd', reason: 'other' })
    await expect.poll(async () => (await d.api('/api/v1/sessions/claude-code%3Aev-1/changes')).after?.trigger, { timeout: 20_000 }).toBe('session_end')
  })
  test.afterAll(async () => { await d.stop() })

  test('the Files view separates reported, observed and unverified changes', async ({ page }) => {
    await d.signIn(page)
    await page.goto(`${d.base}/#/files?view=verified`)
    const row = (name: string) => page.getByRole('row', { name: new RegExp(name) })
    await expect(row('agent.rs')).toContainText('Reported + observed', { timeout: 10_000 })
    await expect(row('claimed.rs')).toContainText('Reported, not seen')
    await expect(row('person.rs')).toContainText('Observed, unreported')
    await expect(page.getByText(/not proof of authorship/)).toBeVisible()
  })

  test('session Git changes credit only reported, in-session changes', async ({ page }) => {
    await d.signIn(page)
    await page.goto(`${d.base}/#/sessions/${encodeURIComponent('claude-code:ev-1')}?tab=changes`)
    await expect(page.getByRole('row', { name: /agent\.rs/ })).toContainText('Reported by this session', { timeout: 10_000 })
    await expect(page.getByRole('row', { name: /person\.rs/ })).toContainText('Unreported')
    await expect(page.getByRole('row', { name: /claimed\.rs/ })).toHaveCount(0)
    const pre = page.locator('.card', { has: page.getByRole('heading', { name: 'Pre-existing changes' }) })
    await expect(pre).toContainText('old.rs')
    await expect(page.getByText(/not who changed them/)).toBeVisible()
  })

  test('integrations shows what is observed', async ({ page }) => {
    await d.signIn(page)
    await page.goto(`${d.base}/#/integrations`)
    const card = page.locator('.card', { hasText: 'File & Git observation' })
    await expect(card).toContainText('observing')
    await expect(card).toContainText(/snapshot/)
  })
})
