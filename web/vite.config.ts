import { defineConfig, type ProxyOptions } from 'vite'
import preact from '@preact/preset-vite'

// `npm run dev` serves the UI from Vite on 127.0.0.1:5173 and proxies API
// calls to a daemon (default 127.0.0.1:7878, or $TAILFLOW_URL).
//
// The proxy holds no credential. Sign in the normal way: open the link
// `tailflow-daemon dashboard --no-open` prints (on 127.0.0.1:7878). Browsers
// send 127.0.0.1 cookies to every port, so the dev page on 127.0.0.1:5173
// then carries the same session cookie, and the daemon applies the same
// checks as for the dashboard it serves: the browser's own Sec-Fetch-Site
// is forwarded unchanged (a page on another port is refused), and only
// Origin, which names the dev port, is dropped.
const target = process.env.TAILFLOW_URL ?? 'http://127.0.0.1:7878'

const proxy: ProxyOptions = {
  target,
  changeOrigin: true,
  configure: p => {
    p.on('proxyReq', req => { req.removeHeader('origin') })
  },
}

export default defineConfig({
  plugins: [preact()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    host: '127.0.0.1',
    cors: false,
    proxy: {
      '/events': proxy,
      '/api': proxy,
      '/health': proxy,
    },
  },
})
