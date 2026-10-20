import { defineConfig, type Plugin } from 'vitest/config'
import vue from '@vitejs/plugin-vue'

// The dev server proxies the API (including console WebSockets) to a
// controller on 127.0.0.1:18080 (deploy/dev/controller.toml).
const controller = process.env.MST_API ?? 'http://127.0.0.1:18080'

// `--mode demo`: the app with a backend of its own in the page (src/demo),
// as static files that work from any path on any host.
const demo: Plugin = {
  name: 'metastratum-demo',
  // Before Vite reads the page's scripts, so the build starts from the demo's.
  transformIndexHtml: {
    order: 'pre',
    handler: (html) =>
      html.replace('/src/main.ts', '/src/demo/main.ts').replace('<title>metastratum</title>', '<title>metastratum demo</title>'),
  },
}

export default defineConfig(({ mode }) => ({
  base: mode === 'demo' ? './' : '/',
  plugins: [vue(), ...(mode === 'demo' ? [demo] : [])],
  server: {
    proxy:
      mode === 'demo'
        ? undefined
        : {
            '/api': { target: controller, ws: true, changeOrigin: false },
          },
  },
  // noVNC uses top-level await.
  build: { target: 'es2022', outDir: mode === 'demo' ? 'dist-demo' : 'dist' },
  test: { environment: 'jsdom', include: ['src/**/*.test.ts'] },
}))
