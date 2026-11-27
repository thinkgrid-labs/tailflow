import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'

export default async function globalTeardown() {
  const state = path.join(os.tmpdir(), 'tailflow-e2e-state.json')
  if (!fs.existsSync(state)) return
  const { pid, root } = JSON.parse(fs.readFileSync(state, 'utf8'))
  try { process.kill(pid, 'SIGTERM') } catch { /* already gone */ }
  await new Promise(r => setTimeout(r, 500))
  fs.rmSync(root, { recursive: true, force: true })
  fs.rmSync(state, { force: true })
  fs.rmSync(path.join(os.tmpdir(), 'tailflow-e2e-auth.json'), { force: true })
}
