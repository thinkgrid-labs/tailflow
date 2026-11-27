// Live-log rendering benchmark (M1.5 method, extended for M3).
//
//   node e2e/bench-logs.mjs <daemon> <loadgen> <rate> <seconds> [--ui=m3|legacy]
//        [--idle] [--big] [--expand] [--rows=N]
//
// Starts a memory-only daemon with one loadgen source, waits until the ring
// holds `rows` records (default 5,000), opens the dashboard's log view in
// headless Chrome, and over a steady window reads CDP Performance metrics
// (task/script/layout/style time), JS heap, DOM nodes, rAF frame intervals
// and long tasks. Then: filter keystroke → paint while streaming, and scroll
// responsiveness (frame time while scrolling the feed). Prints one JSON line.

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import net from 'node:net'
import { chromium } from '@playwright/test'
import { E2E_TOKEN, authHeaders } from './fixture.mjs'

const [bin, loadgen, rateS = '200', secondsS = '20'] = process.argv.slice(2)
const flag = n => process.argv.includes(`--${n}`)
const opt = (n, d) => (process.argv.find(a => a.startsWith(`--${n}=`)) ?? `=${d}`).split('=')[1]
const ui = opt('ui', 'm3')
const rows = Number(opt('rows', '5000'))
const idle = flag('idle'), big = flag('big'), expand = flag('expand')
const rate = Number(rateS), seconds = Number(secondsS)
const sleep = ms => new Promise(r => setTimeout(r, ms))
const freePort = () => new Promise(r => { const s = net.createServer().listen(0, () => { const p = s.address().port; s.close(() => r(p)) }) })

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tfbench-'))
const port = await freePort()
const genSecs = idle ? Math.ceil((rows + 200) / rate) : seconds + 60
const extra = big ? ' big=10:2000' : ''
fs.writeFileSync(path.join(dir, 'tailflow.toml'),
  `[[sources.process]]\nlabel = "gen"\ncmd = "${loadgen} ${rate} ${genSecs}${extra}"\n\n[storage]\nenabled = false\n`)
const daemon = spawn(bin, ['--config', path.join(dir, 'tailflow.toml'), '--port', String(port)],
  { stdio: 'ignore', env: { ...process.env, RUST_LOG: 'error', TAILFLOW_DATA_DIR: path.join(dir, 'data'), TAILFLOW_TOKEN: E2E_TOKEN } })
const base = `http://127.0.0.1:${port}`
// From M4 the API needs a credential (older daemons ignore the header).
for (let i = 0; i < 300; i++) { try { if ((await fetch(`${base}/health`, { headers: authHeaders })).ok) break } catch {} await sleep(50) }
for (let i = 0; i < 1200; i++) {
  const h = await (await fetch(`${base}/health`, { headers: authHeaders })).json()
  if (h.buffered >= rows) break
  await sleep(100)
}

const browser = await chromium.launch({ channel: process.env.PLAYWRIGHT_CHANNEL ?? 'chrome', headless: true })
const page = await browser.newPage({ viewport: { width: 1400, height: 900 } })
await page.addInitScript(() => {
  window.__frames = []; let last = performance.now()
  const loop = t => { window.__frames.push(t - last); last = t; requestAnimationFrame(loop) }
  requestAnimationFrame(loop)
  window.__long = []
  new PerformanceObserver(l => { for (const e of l.getEntries()) window.__long.push(e.duration) }).observe({ type: 'longtask', buffered: true })
})
const cdp = await page.context().newCDPSession(page)
await cdp.send('Performance.enable')
const metrics = async () => Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(m => [m.name, m.value]))

// M4 daemons: sign the browser in with a one-time link (pre-M4: no auth).
const link = await fetch(`${base}/api/v1/auth/login-links`, { method: 'POST', headers: authHeaders })
if (link.ok) await page.goto(`${base}${(await link.json()).path}`)
const t0 = Date.now()
if (ui === 'legacy') {
  await page.goto(`${base}/`)
  await page.waitForFunction(n => document.querySelectorAll('.log-row').length >= n - 100, rows, { timeout: 60_000, polling: 50 })
} else {
  await page.goto(`${base}/#/logs`)
  await page.waitForFunction(n => {
    const note = [...document.querySelectorAll('.note')].find(e => /live lines shown/.test(e.textContent ?? ''))
    const m = note && /of ([\d,]+) live lines/.exec(note.textContent)
    return m && Number(m[1].replace(/,/g, '')) >= n - 100
  }, rows, { timeout: 60_000, polling: 50 })
}
const hydrateMs = Date.now() - t0
await sleep(3000)
if (idle) {
  let last = -1
  for (let i = 0; i < 120; i++) {
    const c = (await (await fetch(`${base}/health`)).json()).cursor
    if (c === last) break
    last = c
    await sleep(2000)
  }
}
if (expand && ui === 'm3') {
  // Expand every rendered JSON row, then keep following.
  for (const b of await page.locator('button[aria-label="Expand JSON"]').all()) await b.click().catch(() => {})
}

const m0 = await metrics()
await page.evaluate(() => { window.__frames = []; window.__long = [] })
await sleep(seconds * 1000)
const m1 = await metrics()
const win = await page.evaluate(() => ({ f: window.__frames, l: window.__long }))
const domRows = await page.evaluate(() => document.querySelectorAll('.log-row').length)
const nodes = await page.evaluate(() => document.getElementsByTagName('*').length)

// Search while streaming: one keystroke → next paint, five times.
const sel = ui === 'legacy' ? '.filter-input' : 'input[aria-label="Filter live log lines"]'
const keystrokes = []
for (const q of ['E', 'ER', 'ERR', 'ERRO', 'ERROR']) {
  keystrokes.push(await page.evaluate(async ([s, v]) => {
    const input = document.querySelector(s)
    const t = performance.now()
    input.value = v
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))
    return performance.now() - t
  }, [sel, q]))
  await sleep(150)
}
await page.evaluate(s => { const i = document.querySelector(s); i.value = ''; i.dispatchEvent(new Event('input', { bubbles: true })) }, sel)
await sleep(500)

// Scroll responsiveness: jump the feed by a page 30 times, time each to paint.
const scroll = await page.evaluate(async legacy => {
  const el = document.querySelector(legacy ? '.feed' : '.feed .vlist')
  const out = []
  for (let i = 0; i < 30; i++) {
    const t = performance.now()
    el.scrollTop = Math.max(0, el.scrollHeight * (0.1 + (i % 10) / 12))
    await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))
    out.push(performance.now() - t)
  }
  return out
}, ui === 'legacy')

await browser.close()
daemon.kill('SIGTERM')
fs.rmSync(dir, { recursive: true, force: true })

const d = k => (m1[k] - m0[k])
const f = win.f.filter(x => x > 0)
const pct = (v, p) => { if (!v.length) return null; const s = [...v].sort((a, b) => a - b); return +s[Math.min(s.length - 1, Math.round((s.length - 1) * p))].toFixed(1) }
console.log(JSON.stringify({
  ui, rate, seconds, idle, big, expand, rows,
  hydrate_ms: hydrateMs, rows_in_dom: domRows, dom_nodes: nodes,
  main_thread_busy_pct: +(100 * d('TaskDuration') / seconds).toFixed(1),
  script_pct: +(100 * d('ScriptDuration') / seconds).toFixed(2),
  layout_pct: +(100 * d('LayoutDuration') / seconds).toFixed(2),
  style_pct: +(100 * d('RecalcStyleDuration') / seconds).toFixed(2),
  js_heap_mb: +(m1.JSHeapUsedSize / 1048576).toFixed(1),
  frames: f.length, frame_p50_ms: pct(f, 0.5), frame_p95_ms: pct(f, 0.95),
  long_tasks: win.l.length, long_task_total_ms: Math.round(win.l.reduce((a, b) => a + b, 0)),
  keystroke_p50_ms: pct(keystrokes, 0.5), keystroke_max_ms: pct(keystrokes, 1),
  scroll_p50_ms: pct(scroll, 0.5), scroll_p95_ms: pct(scroll, 0.95),
}))
