// Spawn a throwaway daemon for tests that need their own (high-volume logs,
// restarts). Memory-only unless `storage` is set; always a private data dir.

import { spawn, type ChildProcess } from 'node:child_process'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import type { Page } from '@playwright/test'
// @ts-expect-error plain ESM helper shared with the manual screenshot script
import { E2E_TOKEN, authHeaders, signInUrl } from './fixture.mjs'

export const binDir = () => path.resolve(process.env.TAILFLOW_BIN_DIR ?? fileURLToPath(new URL('../../target/debug', import.meta.url)))

export function freePort(): Promise<number> {
  return new Promise(resolve => {
    const s = net.createServer().listen(0, '127.0.0.1', () => {
      const p = (s.address() as net.AddressInfo).port
      s.close(() => resolve(p))
    })
  })
}

export interface Daemon {
  proc: ChildProcess
  base: string
  port: number
  dir: string
  stop: (signal?: NodeJS.Signals) => Promise<{ code: number | null; ms: number }>
  restart: () => Promise<Daemon>
  /** Sign `page`'s browser context in to this daemon (one-time link). */
  signIn: (page: Page) => Promise<void>
  /** Authenticated GET (API token), parsed as JSON. */
  api: (p: string) => Promise<any>
}

/** `script` is a /bin/sh program whose stdout becomes the `app` source. */
export async function startDaemon(script: string, opts: { port?: number; dir?: string; storage?: boolean } = {}): Promise<Daemon> {
  const dir = opts.dir ?? fs.mkdtempSync(path.join(os.tmpdir(), 'tf-e2e-d-'))
  const port = opts.port ?? await freePort()
  const sh = path.join(dir, 'app.sh')
  fs.writeFileSync(sh, script)
  const cfg = path.join(dir, 'tailflow.toml')
  fs.writeFileSync(cfg, `[[sources.process]]\nlabel = "app"\ncmd = "sh ${sh}"\n\n[storage]\nenabled = ${opts.storage ? 'true' : 'false'}\n`)
  const proc = spawn(path.join(binDir(), 'tailflow-daemon'), ['--config', cfg, '--port', String(port), '--no-agents'], {
    env: { ...process.env, TAILFLOW_DATA_DIR: path.join(dir, 'data'), TAILFLOW_TOKEN: E2E_TOKEN, RUST_LOG: 'warn' },
    stdio: 'ignore',
  })
  const base = `http://127.0.0.1:${port}`
  for (let i = 0; i < 300; i++) {
    try { if ((await fetch(`${base}/health`)).ok) break } catch { /* starting */ }
    await new Promise(r => setTimeout(r, 50))
  }
  const d: Daemon = {
    proc, base, port, dir,
    stop: (signal = 'SIGTERM') => new Promise(resolve => {
      const t0 = Date.now()
      if (proc.exitCode !== null) return resolve({ code: proc.exitCode, ms: 0 })
      proc.once('exit', code => resolve({ code, ms: Date.now() - t0 }))
      proc.kill(signal)
    }),
    restart: async () => {
      await d.stop()
      return startDaemon(script, { port, dir, storage: opts.storage })
    },
    signIn: async (page: Page) => {
      const res = await page.goto(await signInUrl(base))
      if (!res?.ok()) throw new Error(`sign-in failed: ${res?.status()}`)
    },
    api: async (p: string) => (await fetch(`${base}${p}`, { headers: authHeaders })).json(),
  }
  return d
}
