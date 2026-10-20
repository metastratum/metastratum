import { defineConfig } from '@playwright/test'

// The demo (npm run build:demo first), clicked through as a visitor would:
//   npm run test:demo
export default defineConfig({
  testDir: 'e2e-demo',
  timeout: 2 * 60_000,
  fullyParallel: true,
  outputDir: 'test-results-demo',
  webServer: {
    command: 'npx vite preview --mode demo --host 127.0.0.1 --port 4179 --strictPort',
    url: 'http://127.0.0.1:4179/',
    reuseExistingServer: !process.env.CI,
  },
  use: {
    baseURL: 'http://127.0.0.1:4179/',
    channel: process.env.CI ? undefined : 'chrome',
    trace: 'retain-on-failure',
  },
})
