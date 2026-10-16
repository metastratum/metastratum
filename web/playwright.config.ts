import { defineConfig } from '@playwright/test'

// Browser smoke tests against a running controller and lab:
//   HM_E2E_URL=http://127.0.0.1:18080 HM_E2E_PASSWORD=... npx playwright test
export default defineConfig({
  testDir: 'e2e',
  timeout: 5 * 60_000,
  use: {
    baseURL: process.env.HM_E2E_URL,
    channel: process.env.HM_E2E_CHANNEL ?? 'chrome',
    trace: 'retain-on-failure',
  },
})
