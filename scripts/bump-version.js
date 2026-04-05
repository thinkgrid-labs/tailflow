#!/usr/bin/env node
// Usage: node scripts/bump-version.js <version>
//
// Updates the version field in:
//   - npm/tailflow/package.json        (main package + optionalDependencies)
//   - npm/platforms/<platform>/package.json  (each platform package)
//   - Cargo.toml workspace [package]   (workspace version)
'use strict'

const fs   = require('fs')
const path = require('path')

const version = process.argv[2]
if (!version || !/^\d+\.\d+\.\d+/.test(version)) {
  console.error('Usage: node scripts/bump-version.js <semver>')
  process.exit(1)
}

const ROOT = path.join(__dirname, '..')

// ── npm packages ────────────────────────────────────────────────────────────
const PLATFORM_NAMES = [
  '@thinkgrid/tailflow-darwin-arm64',
  '@thinkgrid/tailflow-darwin-x64',
  '@thinkgrid/tailflow-linux-arm64',
  '@thinkgrid/tailflow-linux-x64',
  '@thinkgrid/tailflow-win32-x64',
]

// Platform packages
const platformDirs = [
  'darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-x64',
]
for (const dir of platformDirs) {
  const pkgPath = path.join(ROOT, 'npm', 'platforms', dir, 'package.json')
  const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'))
  pkg.version = version
  fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n')
  console.log(`updated ${pkgPath}`)
}

// Main package
const mainPath = path.join(ROOT, 'npm', 'tailflow', 'package.json')
const main = JSON.parse(fs.readFileSync(mainPath, 'utf8'))
main.version = version
for (const name of PLATFORM_NAMES) {
  if (main.optionalDependencies?.[name] !== undefined) {
    main.optionalDependencies[name] = version
  }
}
fs.writeFileSync(mainPath, JSON.stringify(main, null, 2) + '\n')
console.log(`updated ${mainPath}`)

// ── Cargo workspace version ──────────────────────────────────────────────────
const cargoPath = path.join(ROOT, 'Cargo.toml')
let cargo = fs.readFileSync(cargoPath, 'utf8')
cargo = cargo.replace(
  /^(version\s*=\s*)"[\d.]+"/m,
  `$1"${version}"`
)
fs.writeFileSync(cargoPath, cargo)
console.log(`updated ${cargoPath}`)

console.log(`\nbumped to ${version}`)
