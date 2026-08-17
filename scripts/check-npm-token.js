#!/usr/bin/env node
// Usage: NPM_TOKEN=npm_xxx node scripts/check-npm-token.js
//
// Diagnoses why `npm publish` fails for the TailFlow packages.
//
// npm reports an unauthorised publish to a scoped package as "E404 Not Found"
// rather than 403 — it will not confirm that a package exists to a caller that
// cannot write to it. That makes a permissions problem look like a missing
// package, and the CLI gives you nothing to tell them apart. This script asks
// the registry the three questions the 404 is hiding:
//
//   1. Is the token valid at all, and which account is it?
//   2. Does that account have write access to the packages we publish?
//   3. Can the token actually see the scope's packages?
'use strict'

const https = require('https')

const REGISTRY = 'registry.npmjs.org'
const SCOPE = '@thinkgrid'
const PACKAGES = [
  '@thinkgrid/tailflow-darwin-arm64',
  '@thinkgrid/tailflow-darwin-x64',
  '@thinkgrid/tailflow-linux-x64',
  '@thinkgrid/tailflow-linux-arm64',
  '@thinkgrid/tailflow-win32-x64',
  'tailflow',
]

const token = process.env.NPM_TOKEN || process.argv[2]

function get(path, withAuth) {
  return new Promise((resolve) => {
    const headers = { accept: 'application/json' }
    if (withAuth) headers.authorization = `Bearer ${token}`
    const req = https.request(
      { host: REGISTRY, path, method: 'GET', headers },
      (res) => {
        let body = ''
        res.on('data', (c) => (body += c))
        res.on('end', () => {
          let json = null
          try { json = JSON.parse(body) } catch { /* not JSON */ }
          resolve({ status: res.statusCode, json, body })
        })
      }
    )
    req.on('error', (e) => resolve({ status: 0, json: null, body: e.message }))
    req.end()
  })
}

const ok   = (m) => console.log(`  \x1b[32m✓\x1b[0m ${m}`)
const bad  = (m) => console.log(`  \x1b[31m✗\x1b[0m ${m}`)
const info = (m) => console.log(`    ${m}`)

async function main() {
  if (!token) {
    console.error('Usage: NPM_TOKEN=npm_xxx node scripts/check-npm-token.js')
    console.error('\nUse the exact value stored in the NPM_TOKEN repository secret.')
    process.exit(2)
  }

  console.log(`\nChecking NPM_TOKEN against ${REGISTRY}\n`)

  // ── 1. Identity ───────────────────────────────────────────────────────────
  console.log('1. Token identity')
  const who = await get('/-/whoami', true)
  if (who.status !== 200 || !who.json?.username) {
    bad(`token is not valid (HTTP ${who.status})`)
    info('It is expired, revoked, or malformed. Mint a new one.')
    process.exit(1)
  }
  const username = who.json.username
  ok(`authenticates as "${username}"`)

  // Token type matters more than permission level. Only classic tokens can
  // enumerate themselves through this endpoint; a granular token is refused.
  // Granular tokens are the ones with a separate package *selection* that can
  // silently exclude a scope, and the ones npm has been migrating people onto
  // — which is how a pipeline that published fine months ago starts failing
  // without anything in the repository changing.
  const tokens = await get('/-/npm/v1/tokens', true)
  if (tokens.status === 200) {
    ok('token type: classic (automation or publish)')
  } else if (tokens.status === 401 || tokens.status === 403) {
    bad(`token type: granular (HTTP ${tokens.status} listing tokens)`)
    info('Granular tokens carry a "Packages and scopes" selection that is')
    info('separate from the read/write permission level. If that selection')
    info(`excludes ${SCOPE}, publish fails with E404 while whoami succeeds.`)
  } else {
    info(`token type: undetermined (HTTP ${tokens.status})`)
  }

  // ── 2. Write access, from the public collaborator map ─────────────────────
  console.log('\n2. Write access to the published packages')
  let identityMismatch = false
  let sawAny = false

  for (const pkg of PACKAGES) {
    const encoded = pkg.replace('/', '%2f')
    const collab = await get(`/-/package/${encoded}/collaborators`, false)
    if (collab.status !== 200 || !collab.json) {
      info(`${pkg}: not published yet (nothing to compare against)`)
      continue
    }
    sawAny = true
    const perms = collab.json
    const mine = perms[username]
    if (mine === 'write') {
      ok(`${pkg}: "${username}" has write`)
    } else {
      bad(`${pkg}: "${username}" has ${mine ? `only "${mine}"` : 'NO access'}`)
      info(`  can write: ${Object.entries(perms)
        .filter(([, v]) => v === 'write')
        .map(([k]) => k)
        .join(', ') || '(nobody listed)'}`)
      identityMismatch = true
    }
  }

  // ── 3. Scope contents (informational) ─────────────────────────────────────
  //
  // This listing is public — the registry serves it without a token — so it
  // describes the org, not the token's rights. It is shown to confirm the
  // scope and package names are what we think they are. A granular token's
  // package *selection* is not exposed by any registry endpoint, so it cannot
  // be probed remotely; that possibility is handled in the verdict below.
  console.log(`\n3. Packages published under ${SCOPE} (public listing)`)
  const listed = await get(`/-/org/${SCOPE.slice(1)}/package?format=cli`, false)
  const ours = listed.json
    ? Object.keys(listed.json).filter((p) => p.includes('tailflow'))
    : []
  if (ours.length) {
    ours.forEach((p) => info(`${p}: ${listed.json[p]}`))
  } else {
    bad(`no tailflow packages found under ${SCOPE} (HTTP ${listed.status})`)
  }

  // ── Verdict ───────────────────────────────────────────────────────────────
  console.log('\n─────────────────────────────────────────────')

  if (identityMismatch) {
    console.log(`VERDICT: wrong account.\n`)
    console.log(`  The token authenticates as "${username}", which does not have`)
    console.log('  write access to these packages. Either mint the token from')
    console.log('  the account shown above as having write, or add')
    console.log(`  "${username}" as a collaborator on them.`)
    process.exit(1)
  }

  if (!sawAny) {
    console.log('VERDICT: inconclusive — none of these packages are published')
    console.log('  yet, so there is no collaborator list to check against.')
    process.exit(1)
  }

  console.log('VERDICT: identity and package permissions are correct.\n')
  console.log(`  "${username}" has write access to every published package, so`)
  console.log('  the account is not the problem. If `npm publish` still returns')
  console.log('  E404, two causes remain — neither is visible to this script:\n')
  console.log('  1. The token is granular and its "Packages and scopes"')
  console.log(`     selection excludes ${SCOPE}. These packages belong to the`)
  console.log('     "thinkgrid" ORGANISATION, not to a personal account, and a')
  console.log('     granular token set to "All packages" covers only packages')
  console.log('     the user owns personally — org-scoped packages must be')
  console.log('     added explicitly by selecting the organisation. Permission')
  console.log('     level "Read and write" does not change which packages it')
  console.log('     applies to. npmjs.com → Access Tokens → your token.')
  console.log('  2. The NPM_TOKEN secret in GitHub is not the token you just')
  console.log('     tested — e.g. it was pasted with a trailing newline, or')
  console.log('     rotated locally but never updated in the repo settings.\n')
  console.log('  Preferred fix: configure npm Trusted Publishing for each')
  console.log('  TailFlow package, using GitHub organisation "thinkgrid-labs",')
  console.log('  repository "tailflow", and workflow "release.yml". The release')
  console.log('  then uses a short-lived OIDC credential and no publish token.')
  console.log('  If retaining token fallback, replace the repository secret with')
  console.log(`  a granular token from "${username}" that explicitly includes`)
  console.log(`  the ${SCOPE} organisation and every TailFlow package.`)
  process.exit(0)
}

main().catch((e) => {
  console.error('check failed:', e.message)
  process.exit(2)
})
