// node --test scripts/release/
'use strict'

const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const rp = require('./release-plan')

const ROOT = path.join(__dirname, '..', '..')
const noSleep = () => {}

// ── Versions and channels ──────────────────────────────────────────────────

test('stable version → npm latest, GitHub latest, not a prerelease', () => {
  assert.deepEqual(rp.releaseChannel('0.4.0'), {
    version: '0.4.0',
    prerelease: false,
    distTag: 'latest',
    makeLatest: true,
  })
  assert.equal(rp.releaseChannel('1.0.0').distTag, 'latest')
  assert.equal(rp.releaseChannel('0.10.12').distTag, 'latest')
})

test('rc version → npm next, GitHub prerelease, never latest', () => {
  assert.deepEqual(rp.releaseChannel('0.4.0-rc.1'), {
    version: '0.4.0-rc.1',
    prerelease: true,
    distTag: 'next',
    makeLatest: false,
  })
  assert.equal(rp.releaseChannel('0.4.0-rc.0').distTag, 'next')
  assert.equal(rp.releaseChannel('0.4.0-rc.12').distTag, 'next')
})

test('beta and alpha versions are prereleases on next', () => {
  for (const v of ['0.4.0-beta.1', '0.4.0-beta.2', '1.0.0-alpha.1']) {
    const c = rp.releaseChannel(v)
    assert.equal(c.prerelease, true, v)
    assert.equal(c.distTag, 'next', v)
    assert.equal(c.makeLatest, false, v)
  }
})

test('malformed versions are rejected', () => {
  const bad = [
    '', ' 0.4.0', '0.4.0 ', '0.4.0\n', '0.4', '0.4.0.1', 'v0.4.0', '00.4.0', '0.04.0', '0.4.00',
    '0.4.0-', '0.4.0-rc', '0.4.0-rc.', '0.4.0-rc.01', '0.4.0-rc.-1', '0.4.0-rc.1.2', '0.4.0-RC.1',
    '0.4.0-rc1', '0.4.0-preview.1', '0.4.0-next.1', '0.4.0-latest', '0.4.0+build.1',
    '0.4.0-rc.1+build.1', 'latest', 'next', undefined, null, 4,
  ]
  for (const v of bad) {
    assert.equal(rp.parseVersion(v), null, JSON.stringify(v))
    assert.throws(() => rp.releaseChannel(v), /not a supported release version/, JSON.stringify(v))
  }
})

test('tags must be "v" + a supported version', () => {
  assert.equal(rp.versionFromTag('v0.4.0-rc.1'), '0.4.0-rc.1')
  assert.equal(rp.versionFromTag('v0.4.0'), '0.4.0')
  for (const t of ['0.4.0', 'V0.4.0', 'v', 'refs/tags/v0.4.0', 'v0.4.0-rc', 'vv0.4.0', undefined, '']) {
    assert.throws(() => rp.versionFromTag(t), undefined, JSON.stringify(t))
  }
})

// ── Tree consistency ───────────────────────────────────────────────────────

function tree(version, edit = () => {}) {
  const t = {
    cargo: version,
    cargoLock: Object.fromEntries(
      ['tailflow-agent', 'tailflow-core', 'tailflow-daemon', 'tailflow-storage', 'tailflow-tui'].map((n) => [n, version])
    ),
    main: {
      name: 'tailflow',
      version,
      optionalDependencies: Object.fromEntries(rp.PLATFORMS.map((p) => [rp.platformPackage(p), version])),
    },
    platforms: Object.fromEntries(rp.PLATFORMS.map((p) => [p, { name: rp.platformPackage(p), version }])),
  }
  edit(t)
  return t
}
const withNotes = () => true
const withoutNotes = () => false

test('consistent rc tree with notes → plan for next', () => {
  const plan = rp.planRelease({ tag: 'v0.4.0-rc.1', tree: tree('0.4.0-rc.1'), notesExist: withNotes })
  assert.equal(plan.distTag, 'next')
  assert.equal(plan.prerelease, true)
  assert.equal(plan.makeLatest, false)
  assert.equal(plan.notesPath, 'docs/releases/v0.4.0-rc.1.md')
})

test('a prerelease without release notes is refused', () => {
  assert.throws(
    () => rp.planRelease({ tag: 'v0.4.0-rc.1', tree: tree('0.4.0-rc.1'), notesExist: withoutNotes }),
    /docs\/releases\/v0\.4\.0-rc\.1\.md is missing/
  )
})

test('a stable release does not require release notes', () => {
  const plan = rp.planRelease({ tag: 'v0.4.0', tree: tree('0.4.0'), notesExist: withoutNotes })
  assert.equal(plan.distTag, 'latest')
  assert.equal(plan.notesPath, '')
})

test('tag that does not match the tree is refused, listing every mismatch', () => {
  // The PR #33 case: an rc tag on a tree still at the previous version.
  assert.throws(
    () => rp.planRelease({ tag: 'v0.4.0-rc.1', tree: tree('0.4.0'), notesExist: withNotes }),
    (e) => {
      assert.match(e.message, /Cargo\.toml \[workspace\.package\] version is "0\.4\.0"/)
      assert.match(e.message, /Cargo\.lock tailflow-core is "0\.4\.0"/)
      assert.match(e.message, /npm\/tailflow\/package\.json version/)
      assert.match(e.message, /npm\/platforms\/win32-x64\/package\.json version/)
      assert.match(e.message, /bump-version\.js 0\.4\.0-rc\.1/)
      return true
    }
  )
  // And the reverse: a stable tag on an rc tree.
  assert.throws(() => rp.planRelease({ tag: 'v0.4.0', tree: tree('0.4.0-rc.1'), notesExist: withNotes }), /expected "0\.4\.0"/)
})

test('one stale manifest is enough to refuse', () => {
  const cases = [
    [(t) => (t.cargo = '0.4.0'), /Cargo\.toml/],
    [(t) => (t.cargoLock['tailflow-tui'] = '0.4.0'), /Cargo\.lock tailflow-tui/],
    [(t) => (t.cargoLock = {}), /Cargo\.lock has no workspace packages/],
    [(t) => (t.main.version = '0.4.0'), /npm\/tailflow\/package\.json version/],
    [(t) => (t.main.optionalDependencies['@thinkgrid/tailflow-linux-arm64'] = '0.4.0'), /optionalDependencies @thinkgrid\/tailflow-linux-arm64/],
    [(t) => delete t.main.optionalDependencies['@thinkgrid/tailflow-darwin-x64'], /optionalDependencies @thinkgrid\/tailflow-darwin-x64 is null/],
    [(t) => (t.main.optionalDependencies['@thinkgrid/tailflow-freebsd-x64'] = '0.4.0-rc.1'), /unexpected package @thinkgrid\/tailflow-freebsd-x64/],
    [(t) => (t.platforms['darwin-arm64'].version = '0.3.3'), /npm\/platforms\/darwin-arm64\/package\.json version is "0\.3\.3"/],
    [(t) => (t.platforms['linux-x64'].name = '@thinkgrid/tailflow-linux'), /name is "@thinkgrid\/tailflow-linux"/],
    [(t) => (t.main.name = 'tailflow-cli'), /name is "tailflow-cli"/],
  ]
  for (const [edit, re] of cases) {
    assert.throws(() => rp.planRelease({ tag: 'v0.4.0-rc.1', tree: tree('0.4.0-rc.1', edit), notesExist: withNotes }), re)
  }
})

test('the committed tree is internally consistent', () => {
  const t = rp.readTree(ROOT)
  assert.ok(rp.parseVersion(t.cargo), `Cargo.toml version ${t.cargo} is not a supported release version`)
  assert.deepEqual(rp.versionMismatches(t, t.cargo), [])
  assert.deepEqual(Object.keys(t.cargoLock).sort(), [
    'tailflow-agent', 'tailflow-core', 'tailflow-daemon', 'tailflow-storage', 'tailflow-tui',
  ])
  if (rp.releaseChannel(t.cargo).prerelease) {
    assert.ok(fs.existsSync(path.join(ROOT, rp.releaseNotesPath(t.cargo))), `${rp.releaseNotesPath(t.cargo)} is missing`)
  }
})

test('Cargo.toml and Cargo.lock parsing', () => {
  const toml = '[package]\nversion = "9.9.9"\n\n[workspace.package]\nedition = "2021"\nversion = "0.4.0-rc.1"\n[x]\nversion = "1"\n'
  assert.equal(rp.cargoWorkspaceVersion(toml), '0.4.0-rc.1')
  assert.equal(rp.cargoWorkspaceVersion('[workspace]\nmembers = []\n'), null)
  const lock = [
    'version = 4', '',
    '[[package]]', 'name = "serde"', 'version = "1.0.0"', 'source = "registry+https://github.com/rust-lang/crates.io-index"', '',
    '[[package]]', 'name = "tailflow-core"', 'version = "0.4.0-rc.1"', 'dependencies = [', ' "serde",', ']', '',
  ].join('\n')
  assert.deepEqual(rp.cargoLockLocalVersions(lock), { 'tailflow-core': '0.4.0-rc.1' })
})

// ── Publication ────────────────────────────────────────────────────────────

/**
 * An in-memory registry with npm's relevant behaviour: `publish` without
 * --tag tags the version `latest` (npm 10) and a dist-tag is set atomically
 * with the publish. `lag` hides the newest writes from the next N views.
 */
function fakeRegistry(packages = {}, { version, lag = 0, failView = null } = {}) {
  const reg = JSON.parse(JSON.stringify(packages))
  const calls = []
  // name → { views left before the last publish shows, state before it }
  const stale = {}
  const nameForDir = (dir) =>
    path.basename(dir) === 'tailflow' ? 'tailflow' : rp.platformPackage(path.basename(dir))
  const lookup = (name) => {
    if (stale[name]?.views > 0) {
      stale[name].views--
      return stale[name].before
    }
    return reg[name]
  }
  const npm = (args, opts = {}) => {
    calls.push({ args, cwd: opts.cwd })
    if (args[0] === 'view') {
      if (failView) return { status: 1, stdout: '', stderr: failView }
      const spec = args[1]
      if (args[2] === 'dist-tags') {
        const p = lookup(spec)
        return p ? { status: 0, stdout: JSON.stringify(p.tags), stderr: '' } : { status: 1, stdout: '', stderr: 'npm error code E404' }
      }
      const at = spec.lastIndexOf('@')
      const p = lookup(spec.slice(0, at))
      return p && p.versions.includes(spec.slice(at + 1))
        ? { status: 0, stdout: JSON.stringify(spec.slice(at + 1)), stderr: '' }
        : { status: 1, stdout: '', stderr: 'npm error code E404\nnpm error 404 No match found' }
    }
    if (args[0] === 'publish') {
      if (args.includes('--dry-run')) return { status: 0, stdout: '', stderr: '' }
      const name = nameForDir(opts.cwd)
      const i = args.indexOf('--tag')
      const tag = i >= 0 ? args[i + 1] : 'latest'
      stale[name] = { views: lag, before: reg[name] && JSON.parse(JSON.stringify(reg[name])) }
      reg[name] = reg[name] ?? { versions: [], tags: {} }
      reg[name].versions.push(version)
      reg[name].tags[tag] = version
      return { status: 0, stdout: '', stderr: '' }
    }
    throw new Error(`unexpected npm ${args.join(' ')}`)
  }
  return { npm, calls, reg }
}

const stableAt = (v) =>
  Object.fromEntries([...rp.PLATFORMS.map(rp.platformPackage), 'tailflow'].map((n) => [n, { versions: [v], tags: { latest: v } }]))

const plan = (version) => ({ ...rp.releaseChannel(version), tag: `v${version}` })
const publishes = (calls) => calls.filter((c) => c.args[0] === 'publish')

test('rc publish: every package with --tag next, platforms first, latest untouched', () => {
  const { npm, calls, reg } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1' })
  rp.publishAll({ plan: plan('0.4.0-rc.1'), npm, log: () => {}, sleep: noSleep })
  const pubs = publishes(calls)
  assert.equal(pubs.length, 6)
  for (const c of pubs) {
    assert.deepEqual(c.args.slice(0, 5), ['publish', '--access', 'public', '--tag', 'next'])
    assert.ok(!c.args.includes('latest'))
  }
  assert.deepEqual(pubs.map((c) => path.basename(c.cwd)), [...rp.PLATFORMS, 'tailflow'])
  for (const [name, p] of Object.entries(reg)) {
    assert.equal(p.tags.latest, '0.3.3', name)
    assert.equal(p.tags.next, '0.4.0-rc.1', name)
  }
})

test('stable publish: every package with --tag latest', () => {
  const { npm, calls, reg } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0' })
  rp.publishAll({ plan: plan('0.4.0'), npm, log: () => {}, sleep: noSleep })
  for (const c of publishes(calls)) assert.deepEqual(c.args.slice(3, 5), ['--tag', 'latest'])
  for (const p of Object.values(reg)) assert.equal(p.tags.latest, '0.4.0')
})

test('a plan whose dist-tag contradicts its version is refused before npm runs', () => {
  const { npm, calls } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1' })
  const forged = [
    { ...plan('0.4.0-rc.1'), distTag: 'latest' },
    { ...plan('0.4.0-rc.1'), prerelease: false, distTag: 'latest' },
    { ...plan('0.4.0'), distTag: 'next' },
    { ...plan('0.4.0-beta.1'), distTag: 'beta' },
  ]
  for (const p of forged) {
    assert.throws(() => rp.publishAll({ plan: p, npm, log: () => {}, sleep: noSleep }), /refusing to publish/)
  }
  assert.equal(calls.length, 0)
})

test('provenance and dry-run flags are passed through; dry-run skips registry checks', () => {
  const { npm, calls, reg } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1' })
  rp.publishAll({ plan: plan('0.4.0-rc.1'), npm, dryRun: true, provenance: true, log: () => {}, sleep: noSleep })
  const pubs = publishes(calls)
  assert.equal(pubs.length, 6)
  for (const c of pubs) {
    assert.ok(c.args.includes('--dry-run'))
    assert.ok(c.args.includes('--provenance'))
    assert.ok(c.args.includes('next'))
  }
  assert.equal(reg.tailflow.tags.next, undefined)
})

test('a retry skips packages already published and completes the rest', () => {
  const start = stableAt('0.3.3')
  for (const p of ['darwin-arm64', 'darwin-x64']) {
    start[rp.platformPackage(p)] = { versions: ['0.3.3', '0.4.0-rc.1'], tags: { latest: '0.3.3', next: '0.4.0-rc.1' } }
  }
  const { npm, calls, reg } = fakeRegistry(start, { version: '0.4.0-rc.1' })
  const actions = rp.publishAll({ plan: plan('0.4.0-rc.1'), npm, log: () => {}, sleep: noSleep })
  assert.deepEqual(actions.map((a) => a.action), ['skip', 'skip', 'publish', 'publish', 'publish', 'publish'])
  assert.equal(publishes(calls).length, 4)
  assert.equal(reg.tailflow.tags.next, '0.4.0-rc.1')
})

test('the main package is not published while a platform package is missing', () => {
  const { npm, calls } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1' })
  // A registry that accepts the publish of win32-x64 but never shows it.
  const lossy = (args, opts) => {
    if (args[0] === 'publish' && opts.cwd.endsWith('win32-x64')) return { status: 0, stdout: '', stderr: '' }
    return npm(args, opts)
  }
  assert.throws(
    () => rp.publishAll({ plan: plan('0.4.0-rc.1'), npm: lossy, log: () => {}, sleep: noSleep, attempts: 3 }),
    /not publishing tailflow@0\.4\.0-rc\.1: @thinkgrid\/tailflow-win32-x64 not on the registry/
  )
  assert.ok(!publishes(calls).some((c) => path.basename(c.cwd) === 'tailflow'))
})

test('registry lag right after a publish is waited out', () => {
  const { npm } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1', lag: 2 })
  let slept = 0
  rp.publishAll({ plan: plan('0.4.0-rc.1'), npm, log: () => {}, sleep: () => slept++, attempts: 5 })
  assert.ok(slept > 0)
})

test('a prerelease that moved latest fails with the restore command', () => {
  const { npm } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1' })
  // Simulates a publish that ignored --tag (npm 10 without --tag sets latest).
  const untagged = (args, opts) =>
    npm(args[0] === 'publish' ? args.filter((a, i) => a !== '--tag' && args[i - 1] !== '--tag') : args, opts)
  assert.throws(
    () => rp.publishAll({ plan: plan('0.4.0-rc.1'), npm: untagged, log: () => {}, sleep: noSleep, attempts: 2 }),
    (e) => {
      assert.match(e.message, /"latest" moved from 0\.3\.3 to 0\.4\.0-rc\.1/)
      assert.match(e.message, /npm dist-tag add tailflow@0\.3\.3 latest/)
      assert.match(e.message, /dist-tag "next" is unset/)
      return true
    }
  )
})

test('a registry error that is not a 404 stops publication', () => {
  const { npm, calls } = fakeRegistry(stableAt('0.3.3'), { version: '0.4.0-rc.1', failView: 'npm error code ETIMEDOUT' })
  assert.throws(() => rp.publishAll({ plan: plan('0.4.0-rc.1'), npm, log: () => {}, sleep: noSleep }), /ETIMEDOUT/)
  assert.equal(publishes(calls).length, 0)
})

// ── Binaries ───────────────────────────────────────────────────────────────

test('binary --version must report the exact version', () => {
  const run = (out, status = 0) => (file) => ({ status, stdout: `${path.basename(file).replace('.exe', '')} ${out}\n`, stderr: '' })
  assert.deepEqual(rp.binaryVersionMismatches('/bin', '0.4.0-rc.1', run('0.4.0-rc.1')), [])
  assert.equal(rp.binaryVersionMismatches('/bin', '0.4.0', run('0.4.0-rc.1')).length, 4)
  assert.equal(rp.binaryVersionMismatches('/bin', '0.4.0-rc.1', run('0.4.0')).length, 4)
  assert.match(rp.binaryVersionMismatches('/bin', '0.4.0', run('0.4.0', 2))[0], /exited 2/)
})

test('staged binaries: missing, empty and non-executable files are reported', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tailflow-release-'))
  try {
    for (const p of rp.PLATFORMS) {
      fs.mkdirSync(path.join(dir, 'npm', 'platforms', p, 'bin'), { recursive: true })
      for (const b of rp.BINARIES) {
        const f = path.join(dir, 'npm', 'platforms', p, 'bin', p.startsWith('win32') ? `${b}.exe` : b)
        fs.writeFileSync(f, 'x')
        fs.chmodSync(f, 0o755)
      }
    }
    assert.deepEqual(rp.missingBinaries(dir), [])
    fs.rmSync(path.join(dir, 'npm/platforms/linux-arm64/bin/tailflow-mcp'))
    fs.writeFileSync(path.join(dir, 'npm/platforms/win32-x64/bin/tailflow.exe'), '')
    const problems = rp.missingBinaries(dir)
    assert.ok(problems.some((p) => /linux-arm64\/bin\/tailflow-mcp is missing/.test(p)))
    assert.ok(problems.some((p) => /win32-x64\/bin\/tailflow\.exe is empty/.test(p)))
    if (process.platform !== 'win32') {
      fs.chmodSync(path.join(dir, 'npm/platforms/darwin-x64/bin/tailflow-logs'), 0o644)
      assert.ok(rp.missingBinaries(dir).some((p) => /darwin-x64\/bin\/tailflow-logs is not executable/.test(p)))
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

// ── bump-version.js ────────────────────────────────────────────────────────

test('bump-version handles prerelease → stable → prerelease and rejects malformed versions', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tailflow-bump-'))
  try {
    for (const rel of ['Cargo.toml', 'Cargo.lock', 'npm', 'scripts/bump-version.js', 'scripts/release/release-plan.js']) {
      fs.cpSync(path.join(ROOT, rel), path.join(dir, rel), { recursive: true })
    }
    const bump = (v) => spawnSync(process.execPath, [path.join(dir, 'scripts/bump-version.js'), v], { encoding: 'utf8' })
    for (const v of ['0.4.0-rc.1', '0.4.0', '0.4.0-rc.2', '0.4.0-beta.1', '0.5.0']) {
      const r = bump(v)
      assert.equal(r.status, 0, `${v}: ${r.stderr}`)
      assert.deepEqual(rp.versionMismatches(rp.readTree(dir), v), [], v)
    }
    for (const v of ['0.4.0garbage', '0.4', 'v0.4.0', '0.4.0-rc', '']) {
      assert.notEqual(bump(v).status, 0, v)
      assert.deepEqual(rp.versionMismatches(rp.readTree(dir), '0.5.0'), [], `${v} must leave the tree alone`)
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

// ── Workflow guard rails ───────────────────────────────────────────────────

test('release.yml pins actions, scopes permissions and routes npm publish through this script', () => {
  const yml = fs.readFileSync(path.join(ROOT, '.github/workflows/release.yml'), 'utf8')
  const uses = [...yml.matchAll(/^\s*-?\s*uses:\s*(\S+)/gm)].map((m) => m[1])
  assert.ok(uses.length > 0)
  for (const u of uses) assert.match(u, /^[\w.-]+\/[\w.-]+@[0-9a-f]{40}$/, `${u} is not pinned to a commit`)
  assert.match(yml, /^permissions:\s*\n\s+contents: read\s*$/m, 'top-level permissions must be contents: read')
  assert.doesNotMatch(yml, /npm@latest|npm install --global npm\b/)
  assert.match(yml, /cargo install cross --locked --git \S+ --rev [0-9a-f]{40}\s*$/m, 'cross must be pinned to a revision')
  assert.match(yml, /_IMAGE: ghcr\.io\/cross-rs\/aarch64-unknown-linux-gnu@sha256:[0-9a-f]{64}$/m, 'cross image must be pinned')
  assert.match(yml, /^\s+RUST_TOOLCHAIN: \d+\.\d+\.\d+$/m, 'the release toolchain must be an exact version')
  assert.match(yml, /^\s+NODE_VERSION: \d+\.\d+\.\d+$/m, 'the release Node must be an exact version')
  const raw = yml.split('\n').filter((l) => /npm publish/.test(l) && !/^\s*#/.test(l))
  assert.deepEqual(raw, [], 'npm publish must go through release-plan.js, which always passes --tag')
  assert.match(yml, /prerelease:\s*\$\{\{\s*needs\.plan\.outputs\.prerelease\s*\}\}/)
  assert.match(yml, /git merge-base --is-ancestor "\$GITHUB_SHA" origin\/main/, 'the tag must be on main')
  assert.match(yml, /release-plan\.js publish --tag "\$GITHUB_REF_NAME" --provenance$/m)
  assert.match(yml, /release-plan\.js plan --tag "\$GITHUB_REF_NAME"$/m)
  assert.doesNotMatch(yml, /retention-days: 1\b/, 'artifacts must outlive a day so failed jobs can be re-run')
  assert.match(yml, /make_latest:\s*\$\{\{\s*needs\.plan\.outputs\.make_latest\s*\}\}/)
})

test('release.yml macOS builds use a supported runner and keep the minimum macOS version', () => {
  const yml = fs.readFileSync(path.join(ROOT, '.github/workflows/release.yml'), 'utf8')
  // Retired labels are not errors at parse time: the job just never starts.
  const os = [...yml.matchAll(/^\s+os:\s*(\S+)/gm)].map((m) => m[1])
  for (const label of os) assert.doesNotMatch(label, /^macos-1[0-4]\b/, `${label} is a retired runner`)
  for (const target of ['aarch64-apple-darwin', 'x86_64-apple-darwin']) {
    assert.match(yml, new RegExp(`target: ${target}\\n\\s+macos_min: "\\d+\\.\\d+"`), `${target} needs macos_min`)
  }
  assert.match(yml, /MACOSX_DEPLOYMENT_TARGET: \$\{\{ matrix\.macos_min \}\}/)
  assert.match(yml, /name: Binaries keep the minimum macOS version\n\s+if: runner\.os == 'macOS'/)
})

// ── CLI ────────────────────────────────────────────────────────────────────

test('plan CLI writes the GitHub outputs the workflow reads', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tailflow-plan-'))
  try {
    for (const rel of ['Cargo.toml', 'Cargo.lock', 'npm', 'scripts/bump-version.js', 'scripts/release/release-plan.js']) {
      fs.cpSync(path.join(ROOT, rel), path.join(dir, rel), { recursive: true })
    }
    const node = (...args) => spawnSync(process.execPath, args, { cwd: dir, encoding: 'utf8', env: { ...process.env, GITHUB_OUTPUT: '' } })
    assert.equal(node('scripts/bump-version.js', '0.4.0-rc.1').status, 0)
    fs.mkdirSync(path.join(dir, 'docs/releases'), { recursive: true })
    fs.writeFileSync(path.join(dir, 'docs/releases/v0.4.0-rc.1.md'), '# notes\n')
    const out = path.join(dir, 'gh-output')
    const r = node('scripts/release/release-plan.js', 'plan', '--tag', 'v0.4.0-rc.1', '--github-output', out)
    assert.equal(r.status, 0, r.stderr)
    assert.deepEqual(fs.readFileSync(out, 'utf8').trim().split('\n'), [
      'version=0.4.0-rc.1',
      'dist_tag=next',
      'prerelease=true',
      'make_latest=false',
      'notes_path=docs/releases/v0.4.0-rc.1.md',
    ])
    const bad = node('scripts/release/release-plan.js', 'plan', '--tag', 'v0.4.0')
    assert.equal(bad.status, 1)
    assert.match(bad.stderr, /^::error::tag v0\.4\.0 cannot be released/m)
    assert.notEqual(node('scripts/release/release-plan.js', 'plan', '--tag').status, 0, 'a flag without its value is refused')
    assert.notEqual(node('scripts/release/release-plan.js', 'ship', '--tag', 'v0.4.0-rc.1').status, 0)
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})
