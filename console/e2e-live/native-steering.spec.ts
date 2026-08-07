import { expect, test } from '@playwright/test'

function requiredEnvironment(name: string): string {
  const value = process.env[name]
  if (!value) throw new Error(`${name} is required`)
  return value
}

test('operator redirects a live native Run through the real control plane', async ({ page }) => {
  const runId = requiredEnvironment('AGENT_RUNTIME_LIVE_RUN_ID')
  const steeringInput = requiredEnvironment('AGENT_RUNTIME_LIVE_STEERING_INPUT')
  const beforeScreenshot = requiredEnvironment('AGENT_RUNTIME_LIVE_BEFORE_SCREENSHOT')
  const afterScreenshot = requiredEnvironment('AGENT_RUNTIME_LIVE_AFTER_SCREENSHOT')
  const runtimeErrors: string[] = []
  const failedApiResponses: string[] = []
  let expectedRateLimitConsoleErrors = 0

  page.on('console', message => {
    if (message.type() !== 'error') return
    if (message.text() ===
      'Failed to load resource: the server responded with a status of 429 (Too Many Requests)') {
      expectedRateLimitConsoleErrors += 1
      return
    }
    runtimeErrors.push(message.text())
  })
  page.on('pageerror', error => runtimeErrors.push(error.message))
  page.on('response', response => {
    const url = new URL(response.url())
    const expectedSteeringRateLimit = response.status() === 429 &&
      url.pathname === `/v1/runs/${runId}:steer`
    if (url.pathname.startsWith('/v1/') && !response.ok() && !expectedSteeringRateLimit) {
      failedApiResponses.push(`${response.status()} ${url.pathname}`)
    }
  })

  await page.goto('/')
  await expect(page.getByRole('heading', { name: '运行中心' })).toBeVisible()
  await expect(page.getByTestId('run-status').first()).toHaveText('运行中')
  const steering = page.getByRole('form', { name: '调整运行指令' }).first()
  await expect(steering).toBeVisible()
  await page.screenshot({ path: beforeScreenshot, fullPage: true })

  await steering.getByRole('textbox', { name: '调整指令' }).fill(steeringInput)
  const response = page.waitForResponse(candidate => {
    const url = new URL(candidate.url())
    return candidate.request().method() === 'POST' &&
      url.pathname === `/v1/runs/${runId}:steer`
  })
  await steering.getByRole('button', { name: '调整', exact: true }).click()
  expect((await response).status()).toBe(202)
  await expect(steering.getByText('指令已受理：', { exact: false })).toBeVisible()

  const limitedResponse = page.waitForResponse(candidate => {
    const url = new URL(candidate.url())
    return candidate.request().method() === 'POST' &&
      url.pathname === `/v1/runs/${runId}:steer` && candidate.status() === 429
  })
  await steering.getByRole('button', { name: '调整', exact: true }).click()
  const limited = await limitedResponse
  expect(limited.headers()['retry-after']).toMatch(/^[12]$/)
  await expect(steering.getByRole('alert')).toContainText(/操作太频繁，请在 [12] 秒后重试/)

  await expect.poll(async () => {
    await page.getByRole('button', { name: '刷新', exact: true }).click()
    return (await page.getByTestId('run-status').first().textContent())?.trim()
  }, { timeout: 45_000, intervals: [250, 500, 1_000] }).toBe('已完成')

  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)
  expect(overflow).toBeLessThanOrEqual(0)
  expect(expectedRateLimitConsoleErrors).toBe(1)
  expect(runtimeErrors).toEqual([])
  expect(failedApiResponses).toEqual([])
  await page.screenshot({ path: afterScreenshot, fullPage: true })
})
