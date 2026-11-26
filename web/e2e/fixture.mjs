// Reproducible multi-agent dataset for browser tests and screenshots.
//
// Every agent event goes through the real hook adapter
// (`tailflow agent hook <agent>`, stdin JSON, exactly as Claude Code and
// Codex invoke it) into a private TAILFLOW_DATA_DIR spool. Hooks carry no
// timestamps, so after each call the spool record's event timestamps are
// shifted to the scripted time — nothing else is edited. A real daemon then
// ingests the spool and runs two runtime log sources.
//
// Synthetic content only: no real activity, paths live under a temp dir.
//
//   node e2e/fixture.mjs <dir with tailflow + tailflow-daemon>   # manual run

import { spawn, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'

const MIN = 60_000

function sh(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', ...opts })
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(' ')} failed: ${r.stderr}`)
  return r.stdout
}

function gitRepo(dir, branch) {
  fs.mkdirSync(path.join(dir, 'src'), { recursive: true })
  fs.writeFileSync(path.join(dir, 'README.md'), '# fixture\n')
  const env = { ...process.env, GIT_AUTHOR_NAME: 'fixture', GIT_AUTHOR_EMAIL: 'fixture@example.invalid', GIT_COMMITTER_NAME: 'fixture', GIT_COMMITTER_EMAIL: 'fixture@example.invalid' }
  sh('git', ['init', '-q', '-b', branch], { cwd: dir, env })
  sh('git', ['add', '.'], { cwd: dir, env })
  sh('git', ['commit', '-q', '-m', 'init'], { cwd: dir, env })
  return () => {
    fs.appendFileSync(path.join(dir, 'README.md'), 'more\n')
    sh('git', ['commit', '-q', '-am', 'change'], { cwd: dir, env })
  }
}

export function freePort() {
  return new Promise(resolve => {
    const s = net.createServer().listen(0, '127.0.0.1', () => {
      const p = s.address().port
      s.close(() => resolve(p))
    })
  })
}

/** Build the dataset. Returns paths and the scripted anchor time. */
export function prepare(binDir, root = fs.mkdtempSync(path.join(os.tmpdir(), 'tf-e2e-'))) {
  const tailflow = path.join(binDir, 'tailflow')
  const data = path.join(root, 'data')
  fs.mkdirSync(data, { recursive: true, mode: 0o700 })
  const api = path.join(root, 'work', 'api-server')
  const web = path.join(root, 'work', 'web-app')
  const commitApi = gitRepo(api, 'main')
  gitRepo(web, 'feature/login')
  const spool = path.join(data, 'agents', 'spool')
  const now = Date.now()
  const T = m => now - (100 - m) * MIN // T(0) = 100 min ago, T(100) = now

  const env = { ...process.env, TAILFLOW_DATA_DIR: data }
  const listing = () => new Set(fs.existsSync(spool) ? fs.readdirSync(spool).filter(f => f.endsWith('.json')) : [])

  /** Run one hook call and stamp its spool record with time `at`. */
  function hook(agent, payload, at) {
    const before = listing()
    const r = spawnSync(tailflow, ['agent', 'hook', agent], { input: typeof payload === 'string' ? payload : JSON.stringify(payload), env, encoding: 'utf8' })
    if (r.status !== 0) throw new Error(`hook ${agent} exited ${r.status}: ${r.stderr}`)
    const added = [...listing()].filter(f => !before.has(f))
    for (const f of added) {
      const p = path.join(spool, f)
      const rec = JSON.parse(fs.readFileSync(p, 'utf8'))
      const iso = new Date(at).toISOString()
      rec.written_at = iso
      for (const e of rec.events ?? []) e.timestamp = iso
      fs.writeFileSync(p, JSON.stringify(rec))
    }
  }

  // ── Claude Code ──────────────────────────────────────────────────────────
  const C = (sid, cwd, name, extra) => ({
    session_id: sid, transcript_path: `/fixture/.claude/projects/x/${sid}.jsonl`, cwd,
    permission_mode: 'default', hook_event_name: name, ...extra,
  })
  const bash = (sid, cwd, id, command, at, end, fail) => {
    hook('claude-code', C(sid, cwd, 'PreToolUse', { tool_name: 'Bash', tool_use_id: id, tool_input: { command } }), at)
    if (fail) hook('claude-code', C(sid, cwd, 'PostToolUseFailure', { tool_name: 'Bash', tool_use_id: id, tool_input: { command }, error: `Exit code ${fail}\nerror: test failed`, is_interrupt: false, duration_ms: end - at }), end)
    else hook('claude-code', C(sid, cwd, 'PostToolUse', { tool_name: 'Bash', tool_use_id: id, tool_input: { command }, tool_response: { stdout: '', stderr: '', interrupted: false }, duration_ms: end - at }), end)
  }
  const read = (sid, cwd, id, file, at) => {
    hook('claude-code', C(sid, cwd, 'PreToolUse', { tool_name: 'Read', tool_use_id: id, tool_input: { file_path: file } }), at)
    hook('claude-code', C(sid, cwd, 'PostToolUse', { tool_name: 'Read', tool_use_id: id, tool_input: { file_path: file }, tool_response: { type: 'text' }, duration_ms: 12 }), at + 400)
  }
  const edit = (sid, cwd, id, file, at, create) => {
    const tool = create ? 'Write' : 'Edit'
    const input = create ? { file_path: file, content: 'x' } : { file_path: file, old_string: 'a', new_string: 'b' }
    hook('claude-code', C(sid, cwd, 'PreToolUse', { tool_name: tool, tool_use_id: id, tool_input: input }), at)
    hook('claude-code', C(sid, cwd, 'PostToolUse', { tool_name: tool, tool_use_id: id, tool_input: input, tool_response: create ? { type: 'create', filePath: file } : { filePath: file }, duration_ms: 30 }), at + 900)
  }

  // C1: api-server, complete session; HEAD changes before it ends.
  const c1 = '11111111-aaaa-4000-8000-000000000001'
  hook('claude-code', C(c1, api, 'SessionStart', { source: 'startup', model: 'claude-opus-5-5' }), T(0))
  hook('claude-code', C(c1, api, 'UserPromptSubmit', { prompt: 'make the config loader tolerate a missing file' }), T(0.5))
  read(c1, api, 'toolu_c1_r1', `${api}/src/config.ts`, T(1))
  edit(c1, api, 'toolu_c1_e1', `${api}/src/config.ts`, T(3))
  bash(c1, api, 'toolu_c1_b1', 'cargo test', T(5), T(7.5), 101)
  hook('claude-code', C(c1, api, 'SubagentStart', { agent_id: 'sub-c1-1', agent_type: 'Explore' }), T(9))
  read(c1, api, 'toolu_c1_r2', `${api}/src/db.rs`, T(10))
  hook('claude-code', C(c1, api, 'SubagentStop', { agent_id: 'sub-c1-1', agent_type: 'Explore' }), T(14))
  edit(c1, api, 'toolu_c1_e2', `${api}/src/db.rs`, T(16))
  bash(c1, api, 'toolu_c1_b2', 'cargo test', T(18), T(21))
  hook('claude-code', C(c1, api, 'Stop', {}), T(22))
  commitApi()
  hook('claude-code', C(c1, api, 'SessionEnd', { reason: 'clear' }), T(40))

  // C3: api-server, joined mid-way (no start observed), idle > 30 min → no end observed.
  const c3 = '33333333-cccc-4000-8000-000000000003'
  read(c3, api, 'toolu_c3_r1', `${api}/src/routes.ts`, T(20))
  bash(c3, api, 'toolu_c3_b1', 'npm run build', T(24), T(26.5))
  edit(c3, api, 'toolu_c3_e1', `${api}/src/routes.ts`, T(30))

  // C2: web-app, active now; a failing test a few seconds before the
  // runtime errors the live sources emit.
  const c2 = '22222222-bbbb-4000-8000-000000000002'
  hook('claude-code', C(c2, web, 'SessionStart', { source: 'startup', model: 'claude-opus-5-5' }), T(50))
  read(c2, web, 'toolu_c2_r1', `${web}/src/App.tsx`, T(52))
  edit(c2, web, 'toolu_c2_e1', `${web}/src/App.tsx`, T(60))
  edit(c2, web, 'toolu_c2_w1', `${web}/src/LoginForm.tsx`, T(70), true)
  bash(c2, web, 'toolu_c2_b1', 'npm test', T(98.5), T(99.6), 1)

  // ── Codex ────────────────────────────────────────────────────────────────
  const X = (sid, cwd, name, extra) => ({ session_id: sid, transcript_path: null, cwd, model: 'gpt-5.5', permission_mode: 'default', hook_event_name: name, ...extra })
  const patch = file => `*** Begin Patch\n*** Update File: ${file}\n@@\n-a\n+b\n*** End Patch`

  // X1: web-app, complete; shell outcome not reported; edits App.tsx too.
  const x1 = '019a0000-0000-7000-8000-00000000000a'
  hook('codex', X(x1, web, 'SessionStart', { source: 'startup' }), T(10))
  hook('codex', X(x1, web, 'UserPromptSubmit', { turn_id: 't1', prompt: 'fix lint' }), T(10.5))
  hook('codex', X(x1, web, 'PreToolUse', { turn_id: 't1', tool_name: 'Bash', tool_use_id: 'call_x1_1', tool_input: { command: 'npm run lint' } }), T(11))
  hook('codex', X(x1, web, 'PostToolUse', { turn_id: 't1', tool_name: 'Bash', tool_use_id: 'call_x1_1', tool_input: { command: 'npm run lint' }, tool_response: { output: '2 problems' } }), T(12))
  hook('codex', X(x1, web, 'PreToolUse', { turn_id: 't1', tool_name: 'apply_patch', tool_use_id: 'call_x1_2', tool_input: { command: patch('src/App.tsx') } }), T(14))
  hook('codex', X(x1, web, 'PostToolUse', { turn_id: 't1', tool_name: 'apply_patch', tool_use_id: 'call_x1_2', tool_input: { command: patch('src/App.tsx') }, tool_response: { output: 'Success' } }), T(14.2))
  hook('codex', X(x1, web, 'PostModelRequest', { turn_id: 't1', thread_id: 'th', request_id: 'r1', attempt_number: 1, kind: 'sampling', status: 'completed', error: null, response_id: 'p1', token_usage: { input_tokens: 5200, cached_input_tokens: 3000, output_tokens: 640, reasoning_output_tokens: 120, total_tokens: 5840 } }), T(15))
  hook('codex', X(x1, web, 'SubagentStart', { turn_id: 't1', agent_id: 'agent-x1', agent_type: 'worker' }), T(16))
  hook('codex', X(x1, web, 'SubagentStop', { turn_id: 't1', agent_id: 'agent-x1', agent_type: 'worker' }), T(25))
  hook('codex', X(x1, web, 'Stop', { turn_id: 't1' }), T(30))
  hook('codex', X(x1, web, 'SessionEnd', { reason: 'other' }), T(35))

  // X2: api-server, concurrent with C1; patches the same file; a command
  // with a reported non-zero exit; never ends.
  const x2 = '019a0000-0000-7000-8000-00000000000b'
  hook('codex', X(x2, api, 'SessionStart', { source: 'startup' }), T(15))
  hook('codex', X(x2, api, 'PreToolUse', { turn_id: 't1', tool_name: 'apply_patch', tool_use_id: 'call_x2_1', tool_input: { command: patch('src/config.ts') } }), T(17))
  hook('codex', X(x2, api, 'PostToolUse', { turn_id: 't1', tool_name: 'apply_patch', tool_use_id: 'call_x2_1', tool_input: { command: patch('src/config.ts') }, tool_response: { output: 'Success' } }), T(17.3))
  hook('codex', X(x2, api, 'PreToolUse', { turn_id: 't1', tool_name: 'Bash', tool_use_id: 'call_x2_2', tool_input: { command: 'make migrate' } }), T(19))
  hook('codex', X(x2, api, 'PostToolUse', { turn_id: 't1', tool_name: 'Bash', tool_use_id: 'call_x2_2', tool_input: { command: 'make migrate' }, tool_response: { output: 'boom', exit_code: 2 } }), T(20))
  hook('codex', X(x2, api, 'Stop', { turn_id: 't1' }), T(45))

  // Loss: one rejected hook input (counted, recorded as a gap).
  hook('claude-code', 'this is not json', T(46))

  // ── Runtime sources ──────────────────────────────────────────────────────
  const apiLog = path.join(root, 'api.sh')
  fs.writeFileSync(apiLog, `#!/bin/sh
echo "INFO  listening on :8080"
echo '{"level":"info","msg":"request","path":"/health","status":200,"ms":3}'
echo "WARN  slow query: 812ms SELECT * FROM sessions"
echo "ERROR connection refused: postgres:5432"
echo "ERROR request failed: GET /api/users 500 (db unavailable)"
echo 'WARN  untrusted <img src=x onerror="window.__xss=1"><script>window.__xss=2</script> payload'
i=0
while true; do
  i=$((i+1))
  echo "INFO  GET /health 200 (tick $i)"
  if [ $((i % 5)) -eq 0 ]; then echo "ERROR connection refused: postgres:5432"; fi
  sleep 2
done
`)
  const webLog = path.join(root, 'web.sh')
  fs.writeFileSync(webLog, `#!/bin/sh
echo "VITE v6.2.6  ready in 412 ms"
echo "  ➜  Local:   http://localhost:5173/"
echo "WARN  [vite] hmr update /src/App.tsx (x2)"
while true; do echo "INFO  [vite] page reload src/LoginForm.tsx"; sleep 7; done
`)
  const config = path.join(root, 'tailflow.toml')
  fs.writeFileSync(config, `[[sources.process]]
label = "api"
cmd = "sh ${apiLog}"

[[sources.process]]
label = "web"
cmd = "sh ${webLog}"
`)
  return { root, data, config, api, web, now, sessions: { c1, c2, c3, x1, x2 } }
}

/** Start a daemon over the prepared dataset and wait until it ingested the spool. */
export async function startDaemon(binDir, fx, port) {
  const daemon = spawn(path.join(binDir, 'tailflow-daemon'), ['--config', fx.config, '--port', String(port)], {
    env: { ...process.env, TAILFLOW_DATA_DIR: fx.data, RUST_LOG: 'warn' },
    stdio: ['ignore', 'ignore', 'pipe'],
  })
  let stderr = ''
  daemon.stderr.on('data', d => { stderr += d })
  const base = `http://127.0.0.1:${port}`
  for (let i = 0; i < 400; i++) {
    try {
      const s = await (await fetch(`${base}/api/v1/sessions?limit=10`)).json()
      if (Array.isArray(s.sessions) && s.sessions.length >= 5) {
        // Let storage commit runtime lines too.
        await new Promise(r => setTimeout(r, 1200))
        return { daemon, base }
      }
    } catch { /* not up yet */ }
    await new Promise(r => setTimeout(r, 100))
  }
  daemon.kill('SIGKILL')
  throw new Error(`daemon did not ingest the fixture spool:\n${stderr}`)
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const binDir = process.argv[2]
  if (!binDir) { console.error('usage: node e2e/fixture.mjs <bin dir> [port]'); process.exit(2) }
  const fx = prepare(binDir)
  const port = Number(process.argv[3]) || await freePort()
  const { base } = await startDaemon(binDir, fx, port)
  console.log(JSON.stringify({ base, root: fx.root, sessions: fx.sessions }))
}
