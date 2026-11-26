import { defineConfig, type ProxyOptions } from 'vite'
import preact from '@preact/preset-vite'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

// `npm run dev` serves the UI from Vite and proxies API calls to a daemon
// (default 127.0.0.1:7878, or $TAILFLOW_URL). From M4 the daemon requires a
// credential: the proxy adds the API token server-side ($TAILFLOW_TOKEN, else
// the daemon's owner-only runtime file), so the dev page never sees it.
const target = process.env.TAILFLOW_URL ?? 'http://127.0.0.1:7878'

function dataDir(): string {
  if (process.env.TAILFLOW_DATA_DIR) return process.env.TAILFLOW_DATA_DIR
  if (process.platform === 'darwin') return path.join(os.homedir(), 'Library/Application Support/tailflow')
  if (process.platform === 'win32') return path.join(process.env.LOCALAPPDATA ?? process.env.APPDATA ?? os.homedir(), 'tailflow')
  const xdg = process.env.XDG_DATA_HOME
  return path.join(xdg && path.isAbsolute(xdg) ? xdg : path.join(os.homedir(), '.local/share'), 'tailflow')
}

/** Read on every request: the token rotates whenever the daemon restarts. */
function token(): string | null {
  if (process.env.TAILFLOW_TOKEN) return process.env.TAILFLOW_TOKEN
  try {
    const port = new URL(target).port || '80'
    const info = JSON.parse(fs.readFileSync(path.join(dataDir(), 'run', `daemon-${port}.json`), 'utf8')) as { token?: unknown }
    return typeof info.token === 'string' ? info.token : null
  } catch {
    return null
  }
}

const proxy: ProxyOptions = {
  target,
  changeOrigin: true,
  configure: p => {
    p.on('proxyReq', (req, incoming) => {
      // Only for the dev page itself: a page on another localhost port that
      // calls this proxy is `same-site`/`cross-site` and gets no token, so
      // the proxy cannot become an unauthenticated gateway to the daemon.
      const site = incoming.headers['sec-fetch-site']
      const t = site === 'same-origin' || site === 'none' ? token() : null
      if (t) req.setHeader('Authorization', `Bearer ${t}`)
      // The bearer token is the credential here; never forward the
      // browser's cookies or Origin to the daemon.
      req.removeHeader('cookie')
      req.removeHeader('origin')
    })
  },
}

export default defineConfig({
  plugins: [preact()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    // Loopback only, and no CORS: the proxy holds the daemon's token.
    host: '127.0.0.1',
    cors: false,
    proxy: {
      '/events': proxy,
      '/api': proxy,
      '/health': proxy,
    },
  },
})
