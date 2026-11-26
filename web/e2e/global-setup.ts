import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import os from 'node:os'
// @ts-expect-error plain ESM helper shared with the manual screenshot script
import { prepare, startDaemon, freePort, signInUrl } from './fixture.mjs'
import { chromium } from '@playwright/test'

export const STATE = path.join(os.tmpdir(), 'tailflow-e2e-state.json')
/** Browser storage (the session cookie) shared by every spec. */
export const AUTH_STATE = path.join(os.tmpdir(), 'tailflow-e2e-auth.json')

export default async function globalSetup() {
  const binDir = path.resolve(process.env.TAILFLOW_BIN_DIR ?? fileURLToPath(new URL('../../target/debug', import.meta.url)))
  for (const b of ['tailflow', 'tailflow-daemon']) {
    if (!fs.existsSync(path.join(binDir, b))) {
      throw new Error(`${b} not found in ${binDir}; build first: cargo build -p tailflow-daemon -p tailflow-tui`)
    }
  }
  const fx = prepare(binDir)
  const port = await freePort()
  const { daemon, base } = await startDaemon(binDir, fx, port)
  process.env.TAILFLOW_E2E_BASE = base
  // Sign in the way a user does: open a one-time link minted with the token.
  const browser = await chromium.launch({ channel: process.env.PLAYWRIGHT_CHANNEL ?? 'chrome' })
  const context = await browser.newContext()
  const page = await context.newPage()
  const res = await page.goto(await signInUrl(base))
  if (!res || !res.ok() || new URL(page.url()).pathname !== '/') {
    throw new Error(`sign-in failed: ${res?.status()} at ${page.url()}`)
  }
  await context.storageState({ path: AUTH_STATE })
  await browser.close()
  fs.writeFileSync(STATE, JSON.stringify({ pid: daemon.pid, base, root: fx.root, binDir, sessions: fx.sessions }))
  daemon.unref()
}
