import { defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'

// The dev server proxies the API (including console WebSockets) to a
// controller started with `hm-controller serve --listen 127.0.0.1:18080`.
const controller = process.env.HM_API ?? 'http://127.0.0.1:18080'

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      '/api': { target: controller, ws: true, changeOrigin: false },
    },
  },
  // noVNC uses top-level await.
  build: { target: 'es2022' },
  test: { environment: 'jsdom', include: ['src/**/*.test.ts'] },
})
