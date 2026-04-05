/**
 * Shared launcher for tailflow and tailflow-daemon.
 *
 * Resolves the compiled binary from the correct platform-specific optional
 * dependency (@tailflow/<platform>) and exec's it, forwarding all args and
 * stdio so that ratatui TUI and TTY interaction work correctly.
 */
'use strict'

const { spawnSync } = require('child_process')
const { join }      = require('path')

// Maps Node.js { platform, arch } → optional dependency name.
const PLATFORM_MAP = {
  'darwin-arm64': '@thinkgrid/tailflow-darwin-arm64',
  'darwin-x64':   '@thinkgrid/tailflow-darwin-x64',
  'linux-arm64':  '@thinkgrid/tailflow-linux-arm64',
  'linux-x64':    '@thinkgrid/tailflow-linux-x64',
  'win32-x64':    '@thinkgrid/tailflow-win32-x64',
}

function run(binaryName) {
  const key = `${process.platform}-${process.arch}`
  const pkg  = PLATFORM_MAP[key]

  if (!pkg) {
    process.stderr.write(
      `tailflow: unsupported platform "${key}"\n` +
      `Supported platforms: ${Object.keys(PLATFORM_MAP).join(', ')}\n`
    )
    process.exit(1)
  }

  // Locate the platform package on disk
  let pkgJsonPath
  try {
    pkgJsonPath = require.resolve(`${pkg}/package.json`)
  } catch {
    process.stderr.write(
      `tailflow: platform package "${pkg}" is not installed.\n` +
      `This usually means the optional dependency was skipped.\n` +
      `Try: npm install ${pkg}\n`
    )
    process.exit(1)
  }

  const ext    = process.platform === 'win32' ? '.exe' : ''
  const binDir = join(pkgJsonPath, '..', 'bin')
  const bin    = join(binDir, `${binaryName}${ext}`)

  const result = spawnSync(bin, process.argv.slice(2), {
    stdio: 'inherit',
    // Pass through the current environment so RUST_LOG etc. work
    env:  process.env,
  })

  if (result.error) {
    // Binary not executable or not found
    process.stderr.write(`tailflow: could not start binary: ${result.error.message}\n`)
    process.exit(1)
  }

  process.exit(result.status ?? 0)
}

module.exports = { run }
