import { defineConfig } from 'vite'
import preact from '@preact/preset-vite'

export default defineConfig({
  plugins: [preact()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    // Proxy API and SSE calls to the daemon during `npm run dev`
    proxy: {
      '/events':      { target: 'http://localhost:7878', changeOrigin: true },
      '/api':         { target: 'http://localhost:7878', changeOrigin: true },
      '/health':      { target: 'http://localhost:7878', changeOrigin: true },
    },
  },
})
