import { defineConfig } from '@playwright/test'
import os from 'node:os'
import path from 'node:path'

// Browser end-to-end tests against a real daemon (see e2e/global-setup.ts).
// Binaries come from TAILFLOW_BIN_DIR (default: the workspace debug build),
// so CI and local runs test exactly what was just built.
//
// Browsers: the `chromium` project uses system Chrome locally (no
// download); in CI set PLAYWRIGHT_CHANNEL=chromium after
// `npx playwright install chromium`. `firefox` and `webkit` need
// `npx playwright install firefox webkit`. Run one with --project.

export default defineConfig({
  testDir: './e2e',
  testMatch: /.*\.spec\.ts/,
  globalSetup: './e2e/global-setup.ts',
  globalTeardown: './e2e/global-teardown.ts',
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  timeout: 30_000,
  expect: { timeout: 8_000 },
  reporter: process.env.CI ? [['list'], ['github']] : 'list',
  use: {
    baseURL: process.env.TAILFLOW_E2E_BASE,
    // Session cookie for the fixture daemon, written by global-setup.
    storageState: path.join(os.tmpdir(), 'tailflow-e2e-auth.json'),
    headless: true,
    viewport: { width: 1440, height: 900 },
    trace: 'retain-on-failure',
  },
  projects: [
    { name: 'chromium', use: { browserName: 'chromium', channel: process.env.PLAYWRIGHT_CHANNEL ?? 'chrome' } },
    { name: 'firefox', use: { browserName: 'firefox' } },
    { name: 'webkit', use: { browserName: 'webkit' } },
  ],
})
