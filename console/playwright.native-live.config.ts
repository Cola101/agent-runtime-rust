import { defineConfig, devices } from '@playwright/test'

const baseURL = process.env.AGENT_RUNTIME_LIVE_CONSOLE_URL
const outputDir = process.env.AGENT_RUNTIME_LIVE_BROWSER_OUTPUT_DIR

if (!baseURL || !outputDir) {
  throw new Error('native live console URL and browser output directory are required')
}

const consoleOrigin = new URL(baseURL)
if (consoleOrigin.protocol !== 'http:' || !['127.0.0.1', 'localhost'].includes(consoleOrigin.hostname)) {
  throw new Error('native live browser tests may only connect to a loopback console')
}

export default defineConfig({
  testDir: './e2e-live',
  outputDir,
  reporter: [['list']],
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  use: {
    baseURL: consoleOrigin.origin,
    channel: 'chrome',
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
  projects: [{
    name: 'native-live',
    use: {
      ...devices['Desktop Chrome HiDPI'],
      viewport: { width: 1440, height: 1000 },
    },
  }],
})
