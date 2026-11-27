#!/usr/bin/env node
// Usage: node scripts/bump-version.js <version>
//
// <version> is MAJOR.MINOR.PATCH, optionally followed by -alpha.N, -beta.N or
// -rc.N (for example 0.4.0 or 0.4.0-rc.1). Updates the version in:
//   - npm/tailflow/package.json               (main package + optionalDependencies pins)
//   - npm/platforms/<platform>/package.json   (each platform package)
//   - Cargo.toml [workspace.package]          (workspace version)
//   - Cargo.lock                              (the workspace members' entries)
//
// Afterwards it runs the release workflow's own consistency check, so a bump
// that leaves any of these behind fails here rather than at tag time.
'use strict'

const fs = require('fs')
const path = require('path')
const release = require('./release/release-plan')

const version = process.argv[2]
if (!release.parseVersion(version)) {
  console.error('Usage: node scripts/bump-version.js <version>')
  console.error('  <version>: MAJOR.MINOR.PATCH, optionally -alpha.N, -beta.N or -rc.N (e.g. 0.4.0, 0.4.0-rc.1)')
  process.exit(1)
}

const ROOT = path.join(__dirname, '..')

const writeJson = (file, value) => fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n')

// ── npm packages ────────────────────────────────────────────────────────────
for (const platform of release.PLATFORMS) {
  const pkgPath = path.join(ROOT, 'npm', 'platforms', platform, 'package.json')
  const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'))
  pkg.version = version
  writeJson(pkgPath, pkg)
  console.log(`updated ${path.relative(ROOT, pkgPath)}`)
}

const mainPath = path.join(ROOT, 'npm', 'tailflow', 'package.json')
const main = JSON.parse(fs.readFileSync(mainPath, 'utf8'))
main.version = version
main.optionalDependencies = main.optionalDependencies ?? {}
for (const platform of release.PLATFORMS) {
  main.optionalDependencies[release.platformPackage(platform)] = version
}
writeJson(mainPath, main)
console.log(`updated ${path.relative(ROOT, mainPath)}`)

// ── Cargo workspace version ─────────────────────────────────────────────────
// Only the `version` key of [workspace.package]; any previous value, including
// a prerelease such as 0.4.0-rc.1, is replaced.
const cargoPath = path.join(ROOT, 'Cargo.toml')
const cargoLines = fs.readFileSync(cargoPath, 'utf8').split('\n')
let table = ''
let replaced = false
for (let i = 0; i < cargoLines.length; i++) {
  const header = /^\s*\[([^\]]+)\]\s*$/.exec(cargoLines[i])
  if (header) table = header[1].trim()
  else if (table === 'workspace.package' && /^\s*version\s*=/.test(cargoLines[i])) {
    cargoLines[i] = cargoLines[i].replace(/"[^"]*"/, `"${version}"`)
    replaced = true
    break
  }
}
if (!replaced) {
  console.error('Cargo.toml has no version in [workspace.package]')
  process.exit(1)
}
fs.writeFileSync(cargoPath, cargoLines.join('\n'))
console.log('updated Cargo.toml')

// ── Cargo.lock workspace entries ────────────────────────────────────────────
// Packages without a `source` line are workspace members. `cargo build
// --locked` refuses a lock file whose member versions are stale.
const lockPath = path.join(ROOT, 'Cargo.lock')
const lock = fs.readFileSync(lockPath, 'utf8')
const parts = lock.split(/^(?=\[\[package\]\]\s*$)/m)
const updated = parts.map((block) => {
  if (!block.startsWith('[[package]]') || /^source = /m.test(block)) return block
  return block.replace(/^version = "[^"]*"/m, `version = "${version}"`)
})
fs.writeFileSync(lockPath, updated.join(''))
console.log('updated Cargo.lock')

const mismatches = release.versionMismatches(release.readTree(ROOT), version)
if (mismatches.length) {
  console.error(`\nversion bump incomplete:\n  - ${mismatches.join('\n  - ')}`)
  process.exit(1)
}
console.log(`\nbumped to ${version}`)
