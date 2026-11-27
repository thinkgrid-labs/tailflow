// Capture every page at several viewports and themes, and report console
// errors and CSP violations. For visual QA (not a pass/fail test).
//   node e2e/screens.mjs <base url> <out dir> [--quick]
// <base url> is a daemon started by `node e2e/fixture.mjs <bin dir>` (it uses
// the fixture's API token to sign each browser context in).

import { chromium } from '@playwright/test'
import fs from 'node:fs'
import path from 'node:path'
import { authHeaders, signInUrl } from './fixture.mjs'

const [base, out] = process.argv.slice(2)
const quick = process.argv.includes('--quick')
fs.mkdirSync(out, { recursive: true })

const sessions = await (await fetch(`${base}/api/v1/sessions?limit=10`, { headers: authHeaders })).json()
const c1 = sessions.sessions.find(s => s.session_id.startsWith('claude-code:1111'))?.session_id ?? sessions.sessions[0]?.session_id

const pages = [
  ['overview', '#/overview'],
  ['sessions', '#/sessions'],
  ['session', `#/sessions/${encodeURIComponent(c1)}`],
  ['session-coverage', `#/sessions/${encodeURIComponent(c1)}?tab=coverage`],
  ['activity', '#/activity'],
  ['errors', '#/errors'],
  ['timeline', '#/timeline?from=' + (Date.now() - 105 * 60e3) + '&to=' + (Date.now() + 60e3)],
  ['logs', '#/logs'],
  ['files', '#/files'],
  ['commands', '#/commands'],
  ['integrations', '#/integrations'],
  ['settings', '#/settings'],
]
const viewports = quick
  ? [['desktop', 1440, 900]]
  : [['desktop', 1440, 900], ['tablet', 900, 1100], ['mobile', 390, 844]]
const themes = quick ? ['dark'] : ['dark', 'light']

const browser = await chromium.launch({ channel: 'chrome', headless: true })
const problems = []
for (const theme of themes) {
  for (const [vp, w, h] of viewports) {
    const ctx = await browser.newContext({ viewport: { width: w, height: h }, colorScheme: theme, deviceScaleFactor: 1 })
    const page = await ctx.newPage()
    await page.goto(await signInUrl(base))
    page.on('console', m => { if (m.type() === 'error' || m.type() === 'warning') problems.push(`${theme}/${vp} console ${m.type()}: ${m.text()}`) })
    page.on('pageerror', e => problems.push(`${theme}/${vp} pageerror: ${e.message}`))
    for (const [name, hash] of pages) {
      await page.goto(`${base}/${hash}`)
      await page.waitForTimeout(name === 'timeline' ? 1800 : 1100)
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1)
      if (overflow) problems.push(`${theme}/${vp}/${name}: horizontal page overflow`)
      await page.screenshot({ path: path.join(out, `${name}-${vp}-${theme}.png`), fullPage: false })
    }
    await ctx.close()
  }
}
await browser.close()
console.log(problems.length ? problems.join('\n') : 'no console errors, CSP violations or page overflow')
