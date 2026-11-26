// What an open dashboard costs the daemon.
//   node e2e/bench-daemon-cost.mjs <bin dir> [seconds=60]
// Starts the fixture daemon (release binaries recommended), then measures the
// daemon's CPU time and RSS over a window with no client, and with each page
// open (visible, polling as it does for a user). Prints one JSON line per case.

import { execFileSync } from 'node:child_process'
import { chromium } from '@playwright/test'
import { prepare, startDaemon, freePort } from './fixture.mjs'

const [binDir, secondsS = '60'] = process.argv.slice(2)
const seconds = Number(secondsS)
const fx = prepare(binDir)
const { daemon, base } = await startDaemon(binDir, fx, await freePort())

function cpuSeconds(pid) {
  const t = execFileSync('ps', ['-o', 'time=', '-p', String(pid)], { encoding: 'utf8' }).trim()
  const parts = t.split(':').map(Number) // [[h:]m:]s.cs
  return parts.reduce((acc, v) => acc * 60 + v, 0)
}
const rssMb = pid => Number(execFileSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' }).trim()) / 1024

async function measure(label, page, hash) {
  if (page && hash) { await page.goto(`${base}/${hash}`); await page.waitForTimeout(3000) }
  const c0 = cpuSeconds(daemon.pid)
  const t0 = Date.now()
  await new Promise(r => setTimeout(r, seconds * 1000))
  const c1 = cpuSeconds(daemon.pid)
  const wall = (Date.now() - t0) / 1000
  console.log(JSON.stringify({ case: label, seconds: wall, daemon_cpu_pct: +(100 * (c1 - c0) / wall).toFixed(3), daemon_rss_mb: +rssMb(daemon.pid).toFixed(1) }))
}

await new Promise(r => setTimeout(r, 5000))
await measure('no client', null, null)
const browser = await chromium.launch({ channel: process.env.PLAYWRIGHT_CHANNEL ?? 'chrome', headless: true })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
for (const [label, hash] of [
  ['overview open', '#/overview'],
  ['sessions open', '#/sessions'],
  ['errors open', '#/errors'],
  ['timeline open (live)', '#/timeline'],
  ['logs open (live SSE)', '#/logs'],
  ['integrations open', '#/integrations'],
]) await measure(label, page, hash)
await browser.close()
daemon.kill('SIGTERM')
