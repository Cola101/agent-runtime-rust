import { expect, test } from '@playwright/test'

function requiredEnvironment(name: string): string {
  const value = process.env[name]
  if (!value) throw new Error(`${name} is required`)
  return value
}

test('reviewer approves a recovered trusted Tool through the real native control plane', async ({ page }) => {
  const runId = requiredEnvironment('AGENT_RUNTIME_LIVE_RUN_ID')
  const approvalId = requiredEnvironment('AGENT_RUNTIME_LIVE_APPROVAL_ID')
  const agentName = requiredEnvironment('AGENT_RUNTIME_LIVE_AGENT_NAME')
  const workspaceName = requiredEnvironment('AGENT_RUNTIME_LIVE_WORKSPACE_NAME')
  const beforeScreenshot = requiredEnvironment('AGENT_RUNTIME_LIVE_BEFORE_SCREENSHOT')
  const afterScreenshot = requiredEnvironment('AGENT_RUNTIME_LIVE_AFTER_SCREENSHOT')
  const runtimeErrors: string[] = []
  const failedApiResponses: string[] = []

  page.on('console', message => {
    if (message.type() === 'error') runtimeErrors.push(message.text())
  })
  page.on('pageerror', error => runtimeErrors.push(error.message))
  page.on('response', response => {
    const url = new URL(response.url())
    if (url.pathname.startsWith('/v1/') && !response.ok()) {
      failedApiResponses.push(`${response.status()} ${url.pathname}`)
    }
  })

  await page.goto('/')

  await expect(page.getByRole('heading', { name: '运行中心' })).toBeVisible()
  const approvals = page.getByRole('region', { name: '待审批 Tool' })
  await expect(approvals.getByRole('heading', { name: '待审批 Tool' })).toBeVisible()
  await expect(approvals.getByText(agentName, { exact: true })).toBeVisible()
  await expect(approvals.getByText(workspaceName, { exact: true })).toBeVisible()
  await expect(approvals.getByText('workspace.read_text', { exact: true })).toBeVisible()
  await expect(approvals.getByText('README.txt', { exact: false })).toBeVisible()
  await expect(approvals.getByText('只读 / 无副作用', { exact: true })).toBeVisible()
  await expect(approvals.getByText('本机可信进程', { exact: true })).toBeVisible()

  await approvals.getByText('查看不可变绑定', { exact: true }).click()
  await expect(approvals.getByText(runId, { exact: true })).toBeVisible()
  await page.screenshot({ path: beforeScreenshot, fullPage: true })

  const allowButton = approvals.getByRole('button', { name: '仅允许本次' })
  expect((await allowButton.boundingBox())?.height).toBeGreaterThanOrEqual(44)
  const approvalResponse = page.waitForResponse(response => {
    const url = new URL(response.url())
    return response.request().method() === 'POST' &&
      url.pathname === `/v1/approvals/${approvalId}:decide`
  })
  await allowButton.click()
  expect((await approvalResponse).status()).toBe(200)

  await expect(approvals.getByText('已允许本次 Tool 执行。', { exact: true })).toBeVisible()
  await expect(approvals.getByText('当前没有待审批项。')).toBeVisible()
  await expect.poll(async () => {
    await page.getByRole('button', { name: '刷新', exact: true }).click()
    return (await page.getByTestId('run-status').first().textContent())?.trim()
  }, { timeout: 45_000, intervals: [250, 500, 1_000] }).toBe('已完成')

  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)
  expect(overflow).toBeLessThanOrEqual(0)
  expect(runtimeErrors).toEqual([])
  expect(failedApiResponses).toEqual([])
  await page.screenshot({ path: afterScreenshot, fullPage: true })
})
