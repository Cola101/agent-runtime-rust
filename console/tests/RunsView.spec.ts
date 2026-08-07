import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, describe, expect, it, vi } from 'vitest'
import RunsView from '../src/views/RunsView.vue'

describe('RunsView', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('creates a real API run from an authorized target and refreshes the visible list', async () => {
    let runListRequests = 0
    vi.stubGlobal('crypto', { randomUUID: () => 'request-view-1' })
    vi.stubGlobal('fetch', vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input)
      if (url === '/v1/console/run-targets') {
        return jsonResponse({ items: [{
          session_id: 'session-1', workspace_id: 'workspace-1', workspace_name: 'Local Workspace',
          agent_version_id: 'agent-version-1', agent_name: 'Local Runtime Agent', agent_version: 1,
          model_policy_id: 'policy-1', model_policy_name: 'Native Model Gateway',
        }] })
      }
      if (url === '/v1/runs') {
        runListRequests += 1
        return jsonResponse({ items: runListRequests === 1 ? [] : [{
          id: 'run-1', workspace_name: 'Local Workspace', agent_name: 'Local Runtime Agent',
          status: 'queued', created_at: '2026-08-02T00:00:00Z',
          budget: { max_tokens: 4000, max_cost_cents: 200, max_duration_seconds: 600 },
        }] })
      }
      if (url === '/v1/approvals?status=pending&limit=50') {
        return jsonResponse({ items: [] })
      }
      if (url === '/v1/sessions/session-1/runs' && init?.method === 'POST') {
        return jsonResponse({ run_id: 'run-1', events_url: '/v1/runs/run-1/events' }, 202)
      }
      return new Response(null, { status: 404 })
    }))
    const wrapper = mount(RunsView)
    await flushPromises()

    const runForm = wrapper.get('section[aria-labelledby="new-run-heading"] form')
    await runForm.get('textarea[name="instructions"]').setValue('Inspect the native runtime')
    await runForm.trigger('submit')
    await flushPromises()

    expect(wrapper.text()).toContain('Run 已受理')
    expect(wrapper.text()).toContain('Local Runtime Agent')
    expect(wrapper.text()).toContain('Local Workspace')
    expect(runListRequests).toBe(2)
  })

  it('provisions an application-scoped Agent configuration and exposes it as a run target', async () => {
    let targetRequests = 0
    vi.stubGlobal('fetch', vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input)
      if (url === '/v1/console/resource-context') {
        return jsonResponse({
          application_id: 'application-1',
          application_name: 'Local Agent Runtime',
          projects: [{ id: 'project-1', name: 'Native Development' }],
        })
      }
      if (url === '/v1/console/run-targets') {
        targetRequests += 1
        return jsonResponse({ items: targetRequests === 1 ? [] : [{
          session_id: 'session-new', workspace_id: 'workspace-new', workspace_name: '开发工作区',
          agent_version_id: 'version-new', agent_name: 'Runtime Agent', agent_version: 1,
          model_policy_id: 'policy-new', model_policy_name: '默认模型策略',
        }] })
      }
      if (url === '/v1/runs') return jsonResponse({ items: [] })
      if (url === '/v1/approvals?status=pending&limit=50') return jsonResponse({ items: [] })
      if (url === '/v1/workspaces' && init?.method === 'POST') {
        return jsonResponse({
          id: 'workspace-new', project_id: 'project-1', name: '开发工作区',
          state: 'ready', created_at: '2026-08-02T00:00:00Z',
        }, 201)
      }
      if (url === '/v1/agents' && init?.method === 'POST') {
        return jsonResponse({
          id: 'agent-new', workspace_id: 'workspace-new', name: 'Runtime Agent',
          created_at: '2026-08-02T00:00:01Z',
        }, 201)
      }
      if (url === '/v1/skills:publish' && init?.method === 'POST') {
        return jsonResponse({
          id: 'skill-version-new', name: 'workspace-review', semantic_version: '1.0.0',
          description: '按受限证据审查工作区内容',
          instructions: '先读取相关文件，引用可验证证据后再给出结论。',
          tool_names: ['workspace.read_text'],
          supported_platforms: ['darwin-arm64', 'linux-arm64', 'linux-x86_64'],
          min_runtime_version: '0.1.0', artifact_digest: 'a'.repeat(64),
          signing_key_id: 'local-skill-key-v1', signature: 'A'.repeat(86),
          created_at: '2026-08-02T00:00:02Z',
        }, 201)
      }
      if (url === '/v1/agents/agent-new/versions' && init?.method === 'POST') {
        return jsonResponse({
          id: 'version-new', agent_id: 'agent-new', version: 1,
          instructions: '先检查工作区证据，再执行任务并报告可验证的结果。',
          delegated_scopes: ['tool:workspace.read'], skill_version_ids: ['skill-version-new'],
          created_at: '2026-08-02T00:00:03Z',
        }, 201)
      }
      if (url === '/v1/model-providers' && init?.method === 'POST') {
        return jsonResponse({
          id: 'provider-new', name: 'Native Provider', protocol: 'openai_compatible',
          endpoint: 'http://127.0.0.1:19090/v1/chat/completions', model: 'native-model',
          state: 'active', credential_status: 'configured', created_at: '2026-08-02T00:00:03Z',
        }, 201)
      }
      if (url === '/v1/model-policies' && init?.method === 'POST') {
        return jsonResponse({
          id: 'policy-new', workspace_id: 'workspace-new', name: '默认模型策略',
          routing: 'single_provider', provider_ids: ['provider-new'], created_at: '2026-08-02T00:00:04Z',
        }, 201)
      }
      if (url === '/v1/sessions' && init?.method === 'POST') {
        return jsonResponse({
          id: 'session-new', workspace_id: 'workspace-new', title: '新会话',
          state: 'active', created_at: '2026-08-02T00:00:04Z',
        }, 201)
      }
      return new Response(null, { status: 404 })
    }))
    const wrapper = mount(RunsView)
    await flushPromises()

    await wrapper.get('input[name="providerName-0"]').setValue('Native Provider')
    await wrapper.get('input[name="providerEndpoint-0"]').setValue('http://127.0.0.1:19090/v1/chat/completions')
    await wrapper.get('input[name="providerModel-0"]').setValue('native-model')
    await wrapper.get('input[name="providerApiKey-0"]').setValue('native-secret')
    await wrapper.get('section[aria-labelledby="runtime-setup-heading"] form').trigger('submit')
    await flushPromises()

    expect(wrapper.text()).toContain('已创建 Runtime Agent v1，可直接启动 Run。')
    expect(wrapper.get('select[name="target"]').text()).toContain('Runtime Agent v1')
    expect(targetRequests).toBe(2)
  })

  it('shows the retry window when a running Run is steered too quickly', async () => {
    vi.stubGlobal('crypto', { randomUUID: () => 'steer-rate-key' })
    vi.stubGlobal('fetch', vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input)
      if (url === '/v1/console/run-targets') return jsonResponse({ items: [] })
      if (url === '/v1/console/resource-context') {
        return jsonResponse({
          application_id: 'application-1', application_name: 'Local Agent Runtime', projects: [],
        })
      }
      if (url === '/v1/runs') {
        return jsonResponse({ items: [{
          id: 'run-running', workspace_name: 'Local Workspace', agent_name: 'Local Runtime Agent',
          status: 'running', created_at: '2026-08-02T00:00:00Z',
          budget: { max_tokens: 4000, max_cost_cents: 200, max_duration_seconds: 600 },
        }] })
      }
      if (url === '/v1/approvals?status=pending&limit=50') return jsonResponse({ items: [] })
      if (url === '/v1/runs/run-running:steer' && init?.method === 'POST') {
        return new Response(JSON.stringify({
          type: 'urn:agent-runtime:problem:run-steering-rate-limit',
          title: 'Run steering rate limit exceeded', status: 429, retry_after_seconds: 2,
        }), {
          status: 429,
          headers: { 'Content-Type': 'application/problem+json', 'Retry-After': '2' },
        })
      }
      return new Response(null, { status: 404 })
    }))
    const wrapper = mount(RunsView)
    await flushPromises()

    await wrapper.get('textarea[name="steeringInput"]').setValue('再次调整')
    await wrapper.get('form[aria-label="调整运行指令"]').trigger('submit')
    await flushPromises()

    expect(wrapper.get('[role="alert"]').text()).toContain('操作太频繁，请在 2 秒后重试')
  })
})

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}
