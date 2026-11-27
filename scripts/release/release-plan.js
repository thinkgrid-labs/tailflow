#!/usr/bin/env node
// Release planning and npm publication for .github/workflows/release.yml.
//
//   node scripts/release/release-plan.js plan    --tag v0.4.0-rc.1 [--github-output FILE]
//   node scripts/release/release-plan.js binaries [--tag v0.4.0-rc.1 --bin-dir DIR]
//   node scripts/release/release-plan.js publish --tag v0.4.0-rc.1 [--dry-run] [--provenance]
//
// `plan` refuses a tag that is not a supported version, or that does not match
// every version in the source tree (Cargo.toml, the Cargo.lock workspace
// entries, the six npm manifests and the main package's platform pins). It
// then derives the release channel from the version alone:
//
//   0.4.0          stable      npm dist-tag `latest`, GitHub release marked latest
//   0.4.0-rc.1     prerelease  npm dist-tag `next`,   GitHub release marked prerelease
//   0.4.0-beta.2   prerelease  npm dist-tag `next`,   GitHub release marked prerelease
//   0.4.0-alpha.1  prerelease  npm dist-tag `next`,   GitHub release marked prerelease
//
// A prerelease never receives `latest`. `publish` passes `--tag` explicitly on
// every `npm publish`, publishes the five platform packages before the main
// package, and afterwards checks the registry: the channel's tag points at
// the new version and, for a prerelease, `latest` did not move.
'use strict'

const fs = require('fs')
const path = require('path')
const { spawnSync } = require('child_process')

const ROOT = path.join(__dirname, '..', '..')

const MAIN_PACKAGE = 'tailflow'
// Publication order: every platform package before the main package, which
// pins them exactly in optionalDependencies.
const PLATFORMS = ['darwin-arm64', 'darwin-x64', 'linux-x64', 'linux-arm64', 'win32-x64']
const BINARIES = ['tailflow', 'tailflow-daemon', 'tailflow-mcp', 'tailflow-logs']
const PRERELEASE_CHANNELS = ['alpha', 'beta', 'rc']
const STABLE_DIST_TAG = 'latest'
const PRERELEASE_DIST_TAG = 'next'

const platformPackage = (platform) => `@thinkgrid/tailflow-${platform}`

// MAJOR.MINOR.PATCH with an optional `-alpha.N`, `-beta.N` or `-rc.N`.
// No leading zeros, no build metadata (npm drops `+…`, so the tag and the
// published version would disagree), nothing else after the patch number.
const VERSION_RE = new RegExp(
  '^(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)' +
    `(?:-(${PRERELEASE_CHANNELS.join('|')})\\.(0|[1-9]\\d*))?$`
)

/** Parses a supported version, or returns null. */
function parseVersion(version) {
  if (typeof version !== 'string') return null
  const m = VERSION_RE.exec(version)
  if (!m) return null
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: Number(m[3]),
    channel: m[4] ?? null,
    number: m[5] === undefined ? null : Number(m[5]),
  }
}

/**
 * The release channel for a version. Throws on anything unsupported, so a
 * caller can never fall through to a default dist-tag.
 */
function releaseChannel(version) {
  const parsed = parseVersion(version)
  if (!parsed) {
    throw new Error(
      `"${version}" is not a supported release version: use MAJOR.MINOR.PATCH, ` +
        `optionally followed by -alpha.N, -beta.N or -rc.N (for example 0.4.0 or 0.4.0-rc.1)`
    )
  }
  const prerelease = parsed.channel !== null
  return {
    version,
    prerelease,
    distTag: prerelease ? PRERELEASE_DIST_TAG : STABLE_DIST_TAG,
    makeLatest: !prerelease,
  }
}

/** `v0.4.0-rc.1` → `0.4.0-rc.1`. Throws unless the tag is `v` + a supported version. */
function versionFromTag(tag) {
  if (typeof tag !== 'string' || !tag.startsWith('v')) {
    throw new Error(`tag "${tag}" must be "v" followed by the version, for example v0.4.0-rc.1`)
  }
  const version = tag.slice(1)
  releaseChannel(version) // validates
  return version
}

/** Notes published as the GitHub release body (prepended to generated notes). */
function releaseNotesPath(version) {
  return `docs/releases/v${version}.md`
}

// ── Reading the source tree ────────────────────────────────────────────────

function readJson(root, rel) {
  return JSON.parse(fs.readFileSync(path.join(root, rel), 'utf8'))
}

/** `version` in Cargo.toml's [workspace.package] table. */
function cargoWorkspaceVersion(cargoToml) {
  let inTable = false
  for (const line of cargoToml.split(/\r?\n/)) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(line)
    if (header) {
      inTable = header[1].trim() === 'workspace.package'
      continue
    }
    const m = inTable && /^\s*version\s*=\s*"([^"]*)"/.exec(line)
    if (m) return m[1]
  }
  return null
}

/** name → version for every Cargo.lock package without a `source` (workspace members). */
function cargoLockLocalVersions(cargoLock) {
  const out = {}
  for (const block of cargoLock.split(/^\[\[package\]\]\s*$/m).slice(1)) {
    const name = /^name = "([^"]+)"/m.exec(block)?.[1]
    const version = /^version = "([^"]+)"/m.exec(block)?.[1]
    if (name && version && !/^source = /m.test(block)) out[name] = version
  }
  return out
}

/** Every version in the tree that a release depends on. */
function readTree(root = ROOT) {
  const main = readJson(root, 'npm/tailflow/package.json')
  const platforms = {}
  for (const p of PLATFORMS) {
    platforms[p] = readJson(root, `npm/platforms/${p}/package.json`)
  }
  return {
    cargo: cargoWorkspaceVersion(fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8')),
    cargoLock: cargoLockLocalVersions(fs.readFileSync(path.join(root, 'Cargo.lock'), 'utf8')),
    main,
    platforms,
  }
}

/** Every place where the tree disagrees with `version`. Empty means consistent. */
function versionMismatches(tree, version) {
  const errors = []
  const expect = (where, actual) => {
    if (actual !== version) errors.push(`${where} is ${JSON.stringify(actual ?? null)}, expected "${version}"`)
  }
  expect('Cargo.toml [workspace.package] version', tree.cargo)
  const locked = Object.entries(tree.cargoLock)
  if (locked.length === 0) errors.push('Cargo.lock has no workspace packages')
  for (const [name, v] of locked) expect(`Cargo.lock ${name}`, v)

  if (tree.main.name !== MAIN_PACKAGE) {
    errors.push(`npm/tailflow/package.json name is "${tree.main.name}", expected "${MAIN_PACKAGE}"`)
  }
  expect('npm/tailflow/package.json version', tree.main.version)
  const pins = tree.main.optionalDependencies ?? {}
  for (const p of PLATFORMS) {
    expect(`npm/tailflow optionalDependencies ${platformPackage(p)}`, pins[platformPackage(p)])
  }
  for (const name of Object.keys(pins)) {
    if (!PLATFORMS.some((p) => platformPackage(p) === name)) {
      errors.push(`npm/tailflow optionalDependencies has unexpected package ${name}`)
    }
  }
  for (const p of PLATFORMS) {
    const pkg = tree.platforms[p]
    if (pkg.name !== platformPackage(p)) {
      errors.push(`npm/platforms/${p}/package.json name is "${pkg.name}", expected "${platformPackage(p)}"`)
    }
    expect(`npm/platforms/${p}/package.json version`, pkg.version)
  }
  return errors
}

/**
 * Validates a tag against the tree and returns the release plan. Throws with
 * every problem listed, so one run shows everything that needs fixing.
 */
function planRelease({ tag, tree, notesExist }) {
  const version = versionFromTag(tag)
  const channel = releaseChannel(version)
  const errors = versionMismatches(tree, version)
  const notes = releaseNotesPath(version)
  if (channel.prerelease && !notesExist(notes)) {
    errors.push(
      `${notes} is missing: a prerelease must ship release notes that say what it is and what is unvalidated`
    )
  }
  if (errors.length) {
    throw new Error(
      `tag ${tag} cannot be released:\n  - ${errors.join('\n  - ')}\n` +
        `Land the version-bump commit (node scripts/bump-version.js ${version}) before tagging.`
    )
  }
  return { ...channel, tag, notesPath: notesExist(notes) ? notes : '' }
}

// ── Staged binaries ────────────────────────────────────────────────────────

/** Missing or non-executable binaries under npm/platforms/<p>/bin. */
function missingBinaries(root = ROOT) {
  const problems = []
  for (const p of PLATFORMS) {
    for (const b of BINARIES) {
      const file = path.join(root, 'npm', 'platforms', p, 'bin', p.startsWith('win32') ? `${b}.exe` : b)
      let st
      try {
        st = fs.statSync(file)
      } catch {
        problems.push(`${path.relative(root, file)} is missing`)
        continue
      }
      if (!st.isFile() || st.size === 0) problems.push(`${path.relative(root, file)} is empty`)
      else if (!p.startsWith('win32') && (st.mode & 0o111) === 0) {
        problems.push(`${path.relative(root, file)} is not executable`)
      }
    }
  }
  return problems
}

// ── npm ────────────────────────────────────────────────────────────────────

function defaultNpm(args, opts = {}) {
  const r = spawnSync('npm', args, {
    cwd: opts.cwd,
    encoding: 'utf8',
    stdio: opts.inherit ? 'inherit' : ['ignore', 'pipe', 'pipe'],
  })
  if (r.error) throw r.error
  return { status: r.status, stdout: r.stdout ?? '', stderr: r.stderr ?? '' }
}

const isNotFound = (r) => /\bE404\b/.test(r.stderr + r.stdout)

/** The package's dist-tags, or {} if the registry has never seen it. */
function distTags(npm, pkg) {
  const r = npm(['view', pkg, 'dist-tags', '--json', '--prefer-online'])
  if (r.status !== 0) {
    if (isNotFound(r)) return {}
    throw new Error(`npm view ${pkg} dist-tags failed: ${r.stderr.trim()}`)
  }
  return r.stdout.trim() ? JSON.parse(r.stdout) : {}
}

/** Whether pkg@version is on the registry. Any error other than a 404 is fatal. */
function isPublished(npm, pkg, version) {
  const r = npm(['view', `${pkg}@${version}`, 'version', '--json', '--prefer-online'])
  if (r.status !== 0) {
    if (isNotFound(r)) return false
    throw new Error(`npm view ${pkg}@${version} failed: ${r.stderr.trim()}`)
  }
  return r.stdout.trim() !== ''
}

function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms)
}

/**
 * The registry is eventually consistent: a version or dist-tag written a
 * moment ago may not be visible yet. Re-evaluates `check` (which returns a
 * list of problems) until it returns none or the attempts run out.
 */
function settle(check, { attempts, delayMs, sleep }) {
  let problems = check()
  for (let i = 1; i < attempts && problems.length; i++) {
    sleep(delayMs)
    problems = check()
  }
  return problems
}

/**
 * Publishes every package for `plan`. Returns the log of actions. With
 * `dryRun`, runs `npm publish --dry-run` and skips the registry checks that
 * only make sense after a real publish.
 */
function publishAll({
  plan,
  root = ROOT,
  npm = defaultNpm,
  dryRun = false,
  provenance = false,
  log = console.log,
  sleep = sleepSync,
  // About five minutes: npm's CDN can serve a stale packument for minutes.
  attempts = 30,
  delayMs = 10000,
}) {
  const retry = { attempts, delayMs, sleep }
  // Defence in depth: the plan is derived from the version, but nothing below
  // may run with a channel that contradicts it.
  const expected = releaseChannel(plan.version)
  if (plan.distTag !== expected.distTag || plan.prerelease !== expected.prerelease) {
    throw new Error(`refusing to publish ${plan.version} with dist-tag "${plan.distTag}" (expected "${expected.distTag}")`)
  }
  if (plan.prerelease && plan.distTag === STABLE_DIST_TAG) {
    throw new Error(`refusing to publish prerelease ${plan.version} as "${STABLE_DIST_TAG}"`)
  }

  const packages = [
    ...PLATFORMS.map((p) => ({ name: platformPackage(p), dir: path.join(root, 'npm', 'platforms', p) })),
    { name: MAIN_PACKAGE, dir: path.join(root, 'npm', 'tailflow') },
  ]
  const before = {}
  for (const { name } of packages) before[name] = distTags(npm, name)

  const actions = []
  for (const { name, dir } of packages) {
    if (name === MAIN_PACKAGE && !dryRun) {
      // The main package is what users install. Publish it only once every
      // platform package it pins exists, so an install never resolves to a
      // version with missing binaries.
      const absent = settle(
        () => PLATFORMS.map(platformPackage).filter((p) => !isPublished(npm, p, plan.version)),
        retry
      )
      if (absent.length) {
        throw new Error(`not publishing ${MAIN_PACKAGE}@${plan.version}: ${absent.join(', ')} not on the registry`)
      }
    }
    if (isPublished(npm, name, plan.version)) {
      log(`${name}@${plan.version} is already published, skipping`)
      actions.push({ name, action: 'skip' })
      continue
    }
    const args = ['publish', '--access', 'public', '--tag', plan.distTag]
    if (provenance) args.push('--provenance')
    if (dryRun) args.push('--dry-run')
    log(`npm ${args.join(' ')}   # ${name}@${plan.version}`)
    const r = npm(args, { cwd: dir, inherit: true })
    if (r.status !== 0) throw new Error(`npm publish failed for ${name}@${plan.version} (exit ${r.status})`)
    actions.push({ name, action: dryRun ? 'dry-run' : 'publish', args })
  }

  if (!dryRun) {
    const problems = settle(() => {
      const found = []
      for (const { name } of packages) {
        const after = distTags(npm, name)
        if (after[plan.distTag] !== plan.version) {
          found.push(`${name}: dist-tag "${plan.distTag}" is ${after[plan.distTag] ?? 'unset'}, expected ${plan.version}`)
        }
        const was = before[name][STABLE_DIST_TAG]
        if (plan.prerelease && after[STABLE_DIST_TAG] !== was) {
          found.push(
            `${name}: "${STABLE_DIST_TAG}" moved from ${was ?? 'unset'} to ${after[STABLE_DIST_TAG]}` +
              (was ? `; restore it with: npm dist-tag add ${name}@${was} ${STABLE_DIST_TAG}` : '')
          )
        }
      }
      return found
    }, retry)
    if (problems.length) throw new Error(`registry check failed after publishing:\n  - ${problems.join('\n  - ')}`)
    log(`registry check: every package has ${plan.distTag} = ${plan.version}` +
      (plan.prerelease ? `; ${STABLE_DIST_TAG} unchanged` : ''))
  }
  return actions
}

// ── CLI ────────────────────────────────────────────────────────────────────

const FLAGS = { '--dry-run': 'dryRun', '--provenance': 'provenance' }
const VALUES = { '--tag': 'tag', '--github-output': 'githubOutput', '--bin-dir': 'binDir' }

function parseArgs(argv) {
  const [command, ...rest] = argv
  const opts = { command }
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]
    if (FLAGS[a]) opts[FLAGS[a]] = true
    else if (VALUES[a] && i + 1 < rest.length) opts[VALUES[a]] = rest[++i]
    else throw new Error(`unknown or incomplete argument: ${a}`)
  }
  return opts
}

/**
 * Problems with the binaries in `binDir`: each must report exactly `version`
 * for `--version` (a whitespace-separated token, so 0.4.0 does not match a
 * 0.4.0-rc.1 binary).
 */
function binaryVersionMismatches(binDir, version, run = defaultRun) {
  const ext = process.platform === 'win32' ? '.exe' : ''
  const problems = []
  for (const b of BINARIES) {
    const file = path.join(binDir, b + ext)
    const r = run(file, ['--version'])
    const out = `${r.stdout}`.trim()
    if (r.status !== 0) problems.push(`${b} --version exited ${r.status}: ${`${r.stderr}`.trim()}`)
    else if (!out.split(/\s+/).includes(version)) problems.push(`${b} --version printed "${out}", expected ${version}`)
  }
  return problems
}

function defaultRun(file, args) {
  const r = spawnSync(file, args, { encoding: 'utf8' })
  if (r.error) return { status: -1, stdout: '', stderr: r.error.message }
  return r
}

function writeOutputs(file, plan) {
  const lines = [
    `version=${plan.version}`,
    `dist_tag=${plan.distTag}`,
    `prerelease=${plan.prerelease}`,
    `make_latest=${plan.makeLatest}`,
    `notes_path=${plan.notesPath}`,
  ]
  fs.appendFileSync(file, lines.join('\n') + '\n')
}

function main(argv) {
  const opts = parseArgs(argv)
  const tag = opts.tag ?? process.env.GITHUB_REF_NAME
  const notesExist = (rel) => fs.existsSync(path.join(ROOT, rel))
  switch (opts.command) {
    case 'plan': {
      const plan = planRelease({ tag, tree: readTree(), notesExist })
      console.log(JSON.stringify(plan, null, 2))
      const out = opts.githubOutput ?? process.env.GITHUB_OUTPUT
      if (out) writeOutputs(out, plan)
      return 0
    }
    case 'binaries': {
      // With --bin-dir: the binaries there report the tag's version.
      // Without: every platform's binaries are staged for publication.
      if (opts.binDir) {
        const version = versionFromTag(tag)
        const problems = binaryVersionMismatches(opts.binDir, version)
        if (problems.length) throw new Error(`binaries do not report ${version}:\n  - ${problems.join('\n  - ')}`)
        console.log(`all ${BINARIES.length} binaries in ${opts.binDir} report ${version}`)
        return 0
      }
      const problems = missingBinaries()
      if (problems.length) throw new Error(`staged binaries are incomplete:\n  - ${problems.join('\n  - ')}`)
      console.log(`all ${PLATFORMS.length * BINARIES.length} binaries are staged`)
      return 0
    }
    case 'publish': {
      const plan = planRelease({ tag, tree: readTree(), notesExist })
      const problems = missingBinaries()
      if (problems.length) throw new Error(`staged binaries are incomplete:\n  - ${problems.join('\n  - ')}`)
      publishAll({ plan, dryRun: !!opts.dryRun, provenance: !!opts.provenance })
      return 0
    }
    default:
      throw new Error(
        'usage: release-plan.js plan|binaries|publish --tag vX.Y.Z[-rc.N] ' +
          '[--github-output FILE] [--bin-dir DIR] [--dry-run] [--provenance]'
      )
  }
}

if (require.main === module) {
  try {
    process.exit(main(process.argv.slice(2)))
  } catch (e) {
    console.error(`::error::${e.message.split('\n')[0]}`)
    console.error(e.message)
    process.exit(1)
  }
}

module.exports = {
  BINARIES,
  MAIN_PACKAGE,
  PLATFORMS,
  PRERELEASE_DIST_TAG,
  STABLE_DIST_TAG,
  binaryVersionMismatches,
  cargoLockLocalVersions,
  cargoWorkspaceVersion,
  missingBinaries,
  parseArgs,
  parseVersion,
  planRelease,
  platformPackage,
  publishAll,
  readTree,
  releaseChannel,
  releaseNotesPath,
  versionFromTag,
  versionMismatches,
}
