import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import os from 'node:os'
// @ts-expect-error plain ESM helper shared with the manual screenshot script
import { prepare, startDaemon, freePort } from './fixture.mjs'

export const STATE = path.join(os.tmpdir(), 'tailflow-e2e-state.json')

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
  fs.writeFileSync(STATE, JSON.stringify({ pid: daemon.pid, base, root: fx.root, binDir, sessions: fx.sessions }))
  daemon.unref()
}
