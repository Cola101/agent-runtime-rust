import { expect, test } from '@playwright/test'

test('run operations page renders authorized tenant data responsively', async ({ page }, testInfo) => {
  const runtimeErrors: string[] = []
  let runCreated = false
  let approvalPending = true
  let configured = false
  let providerRequests = 0
  page.on('console', message => {
    if (message.type() === 'error') runtimeErrors.push(message.text())
  })
  page.on('pageerror', error => runtimeErrors.push(error.message))
  await page.route('**/v1/console/resource-context', route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({
      application_id: '22222222-2222-4222-8222-222222222222',
      application_name: 'Local Agent Runtime',
      projects: [{ id: '33333333-3333-4333-8333-333333333333', name: 'Native Development' }],
    }),
  }))
  await page.route('**/v1/console/run-targets', route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({
      items: [{
        session_id: '77777777-7777-4777-8777-777777777777',
        workspace_id: '44444444-4444-4444-8444-444444444444',
        workspace_name: 'release-workspace',
        agent_version_id: '66666666-6666-4666-8666-666666666666',
        agent_name: 'Release analyst',
        agent_version: 1,
        model_policy_id: '88888888-8888-4888-8888-888888888888',
        model_policy_name: 'Production policy',
      }, ...(configured ? [{
        session_id: 'session-new',
        workspace_id: 'workspace-new',
        workspace_name: '开发工作区',
        agent_version_id: 'version-new',
        agent_name: 'Runtime Agent',
        agent_version: 1,
        model_policy_id: 'policy-new',
        model_policy_name: '默认模型策略',
      }] : [])],
    }),
  }))
  await page.route('**/v1/workspaces', async route => {
    expect(route.request().postDataJSON()).toEqual({
      project_id: '33333333-3333-4333-8333-333333333333',
      name: '开发工作区',
    })
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'workspace-new', project_id: '33333333-3333-4333-8333-333333333333',
        name: '开发工作区', state: 'ready', created_at: '2026-08-02T04:00:01Z',
      }),
    })
  })
  await page.route('**/v1/agents', route => route.fulfill({
    status: 201,
    contentType: 'application/json',
    body: JSON.stringify({
      id: 'agent-new', workspace_id: 'workspace-new', name: 'Runtime Agent',
      created_at: '2026-08-02T04:00:02Z',
    }),
  }))
  await page.route('**/v1/skills:publish', async route => {
    expect(route.request().postDataJSON()).toEqual({
      name: 'workspace-review', semantic_version: '1.0.0',
      description: '按受限证据审查工作区内容',
      instructions: '先读取相关文件，引用可验证证据后再给出结论。',
      tool_names: ['workspace.read_text'],
      supported_platforms: ['darwin-arm64', 'linux-arm64', 'linux-x86_64'],
      min_runtime_version: '0.1.0',
    })
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'skill-version-new', name: 'workspace-review', semantic_version: '1.0.0',
        description: '按受限证据审查工作区内容',
        instructions: '先读取相关文件，引用可验证证据后再给出结论。',
        tool_names: ['workspace.read_text'],
        supported_platforms: ['darwin-arm64', 'linux-arm64', 'linux-x86_64'],
        min_runtime_version: '0.1.0', artifact_digest: 'a'.repeat(64),
        signing_key_id: 'local-skill-key-v1', signature: 'A'.repeat(86),
        created_at: '2026-08-02T04:00:03Z',
      }),
    })
  })
  await page.route('**/v1/agents/agent-new/versions', async route => {
    expect(route.request().postDataJSON()).toEqual({
      instructions: '先检查工作区证据，再执行任务并报告可验证的结果。',
      delegated_scopes: ['tool:workspace.read'], skill_version_ids: ['skill-version-new'],
    })
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'version-new', agent_id: 'agent-new', version: 1,
        instructions: '先检查工作区证据，再执行任务并报告可验证的结果。',
        delegated_scopes: ['tool:workspace.read'], skill_version_ids: ['skill-version-new'],
        created_at: '2026-08-02T04:00:04Z',
      }),
    })
  })
  await page.route('**/v1/model-providers', async route => {
    providerRequests += 1
    const expected = providerRequests === 1 ? {
      name: 'Native Provider', protocol: 'openai_compatible',
      endpoint: 'http://127.0.0.1:19090/v1/chat/completions', model: 'native-model',
      api_key: 'native-write-only-secret',
    } : {
      name: 'Fallback Provider', protocol: 'openai_responses',
      endpoint: 'http://127.0.0.1:19091/v1/responses', model: 'fallback-model',
      api_key: 'fallback-write-only-secret',
    }
    expect(route.request().postDataJSON()).toEqual(expected)
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: `provider-${providerRequests}`, name: expected.name, protocol: expected.protocol,
        endpoint: expected.endpoint, model: expected.model,
        state: 'active', credential_status: 'configured', created_at: '2026-08-02T04:00:04Z',
      }),
    })
  })
  await page.route('**/v1/model-policies', async route => {
    expect(route.request().postDataJSON()).toEqual({
      workspace_id: 'workspace-new',
      name: '默认模型策略',
      routing: 'ordered_failover',
      provider_ids: ['provider-1', 'provider-2'],
    })
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'policy-new', workspace_id: 'workspace-new', name: '默认模型策略',
        routing: 'ordered_failover', provider_ids: ['provider-1', 'provider-2'],
        created_at: '2026-08-02T04:00:05Z',
      }),
    })
  })
  await page.route('**/v1/sessions', async route => {
    configured = true
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'session-new', workspace_id: 'workspace-new', title: '新会话',
        state: 'active', created_at: '2026-08-02T04:00:05Z',
      }),
    })
  })
  await page.route('**/v1/runs', route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({
      items: runCreated ? [{
        id: '0191f761-0ef0-7000-8000-000000000001',
        workspace_name: 'release-workspace',
        agent_name: 'Release analyst',
        status: 'queued',
        created_at: '2026-07-31T00:00:00Z',
        budget: { max_tokens: 4000, max_cost_cents: 200, max_duration_seconds: 600 },
      }] : [],
    }),
  }))
  await page.route('**/v1/approvals?status=pending&limit=50', route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({
      items: approvalPending ? [{
        id: '0191f761-0ef0-7000-8000-000000000002',
        run_id: '0191f761-0ef0-7000-8000-000000000001',
        version: 1,
        status: 'pending',
        workspace_name: 'release-workspace',
        agent_name: 'Release analyst',
        tool_name: 'workspace.read_text',
        tool_call_id: 'call-readme',
        effect: 'pure',
        sandbox: 'trusted_native',
        binding_digest: 'a'.repeat(64),
        arguments: { path: 'README.md' },
        created_at: '2026-08-02T04:00:00Z',
      }] : [],
    }),
  }))
  await page.route('**/v1/approvals/*:decide', async route => {
    expect(route.request().postDataJSON()).toEqual({ version: 1, decision: 'allow_once' })
    approvalPending = false
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        id: '0191f761-0ef0-7000-8000-000000000002',
        tenantId: '11111111-1111-4111-8111-111111111111',
        runId: '0191f761-0ef0-7000-8000-000000000001',
        version: 2,
        status: 'approved',
        createdAt: '2026-08-02T04:00:00Z',
      }),
    })
  })
  await page.route('**/v1/sessions/*/runs', async route => {
    const request = route.request()
    expect(request.headers()['idempotency-key']).toBeTruthy()
    expect(request.postDataJSON().input).toBe('检查 Runtime 健康状态')
    runCreated = true
    await route.fulfill({
      status: 202,
      contentType: 'application/json',
      body: JSON.stringify({
        run_id: '0191f761-0ef0-7000-8000-000000000001',
        events_url: '/v1/runs/0191f761-0ef0-7000-8000-000000000001/events',
      }),
    })
  })

  await page.goto('/')

  await expect(page.getByRole('heading', { name: '运行中心' })).toBeVisible()
  await expect(page.getByText('当前应用：')).toContainText('Local Agent Runtime')
  await page.locator('input[name="providerName-0"]').fill('Native Provider')
  await page.locator('input[name="providerEndpoint-0"]').fill('http://127.0.0.1:19090/v1/chat/completions')
  await page.locator('input[name="providerModel-0"]').fill('native-model')
  await page.locator('input[name="providerApiKey-0"]').fill('native-write-only-secret')
  await page.getByRole('button', { name: '添加备用 Provider' }).click()
  await page.locator('input[name="providerName-1"]').fill('Fallback Provider')
  await page.locator('select[name="providerProtocol-1"]').selectOption('openai_responses')
  await page.locator('input[name="providerEndpoint-1"]').fill('http://127.0.0.1:19091/v1/responses')
  await page.locator('input[name="providerModel-1"]').fill('fallback-model')
  await page.locator('input[name="providerApiKey-1"]').fill('fallback-write-only-secret')
  await page.getByRole('button', { name: '创建并启用' }).click()
  await expect(page.getByText('已创建 Runtime Agent v1，可直接启动 Run。')).toBeVisible()
  await expect(page.locator('input[name="providerApiKey-0"]')).toHaveValue('')
  await expect(page.locator('input[name="providerApiKey-1"]')).toHaveValue('')
  expect(providerRequests).toBe(2)
  await expect(page.getByLabel('Agent 与工作区')).toContainText('Runtime Agent v1')
  await expect(page.getByRole('heading', { name: '待审批 Tool' })).toBeVisible()
  await expect(page.getByText('workspace.read_text', { exact: true })).toBeVisible()
  await expect(page.getByText('README.md', { exact: false })).toBeVisible()
  const allowButton = page.getByRole('button', { name: '仅允许本次' })
  expect((await allowButton.boundingBox())?.height).toBeGreaterThanOrEqual(44)
  await allowButton.click()
  await expect(page.getByText('已允许本次 Tool 执行。', { exact: true })).toBeVisible()
  await expect(page.getByText('当前没有待审批项。')).toBeVisible()
  await page.getByLabel('任务说明').fill('检查 Runtime 健康状态')
  await page.getByRole('button', { name: '启动 Run' }).click()
  await expect(page.getByText(/Run 已受理/)).toBeVisible()
  await expect(page.getByTestId('run-status')).toHaveText('排队中')
  await expect(page.locator('tbody').getByText('release-workspace', { exact: true })).toBeVisible()
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)
  expect(overflow).toBeLessThanOrEqual(0)
  const refreshButton = page.getByRole('button', { name: '刷新', exact: true })
  expect((await refreshButton.boundingBox())?.height).toBeGreaterThanOrEqual(44)
  expect((await page.getByRole('button', { name: '启动 Run' }).boundingBox())?.height)
    .toBeGreaterThanOrEqual(44)
  expect(runtimeErrors).toEqual([])

  await page.screenshot({
    path: `test-results/${testInfo.project.name}-runs.png`,
    fullPage: true,
  })
})
