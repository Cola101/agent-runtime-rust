import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, describe, expect, it, vi } from 'vitest'
import ApprovalInbox from '../src/components/approvals/ApprovalInbox.vue'

const pendingApproval = {
  id: 'approval-1',
  run_id: 'run-1',
  version: 3,
  status: 'pending',
  workspace_name: 'Local Workspace',
  agent_name: 'Local Runtime Agent',
  tool_name: 'workspace.read_text',
  tool_call_id: 'call-readme',
  effect: 'pure',
  sandbox: 'trusted_native',
  binding_digest: 'a'.repeat(64),
  arguments: { path: 'README.md' },
  created_at: '2026-08-02T04:00:00Z',
}

describe('ApprovalInbox', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('shows the reviewed tool binding and removes it after allow-once succeeds', async () => {
    const requests: Array<{ url: string, init?: RequestInit }> = []
    vi.stubGlobal('fetch', vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input)
      requests.push({ url, init })
      if (url === '/v1/approvals?status=pending&limit=50') {
        return jsonResponse({ items: requests.filter(request => request.url === url).length === 1
          ? [pendingApproval]
          : [] })
      }
      if (url === '/v1/approvals/approval-1:decide' && init?.method === 'POST') {
        return jsonResponse({
          id: 'approval-1', tenantId: 'tenant-1', runId: 'run-1', version: 4,
          status: 'approved', createdAt: '2026-08-02T04:00:00Z',
        })
      }
      return new Response(null, { status: 404 })
    }))
    const wrapper = mount(ApprovalInbox)
    await flushPromises()

    expect(wrapper.text()).toContain('Local Runtime Agent')
    expect(wrapper.text()).toContain('Local Workspace')
    expect(wrapper.text()).toContain('workspace.read_text')
    expect(wrapper.text()).toContain('README.md')
    await wrapper.get('[data-testid="approval-allow-once"]').trigger('click')
    await flushPromises()

    const decision = requests.find(request => request.url.endsWith(':decide'))
    expect(decision?.init?.headers).toEqual({
      Accept: 'application/json',
      'Content-Type': 'application/json',
    })
    expect(JSON.parse(String(decision?.init?.body))).toEqual({
      version: 3,
      decision: 'allow_once',
    })
    expect(wrapper.text()).toContain('已允许本次 Tool 执行')
    expect(wrapper.text()).toContain('当前没有待审批项')
  })

  it('offers and submits a session grant only when the server declares it available', async () => {
    const requests: Array<{ url: string, init?: RequestInit }> = []
    vi.stubGlobal('fetch', vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input)
      requests.push({ url, init })
      if (url === '/v1/approvals?status=pending&limit=50') {
        return jsonResponse({
          items: requests.filter(request => request.url === url).length === 1
            ? [{
                ...pendingApproval,
                policy_digest: 'b'.repeat(64),
                session_scope_digest: 'c'.repeat(64),
                policy_snapshot: {
                  approval: 'ask',
                  effect: 'pure',
                  implementation_digest: 'd'.repeat(64),
                  required_scopes: ['workspace:read'],
                  sandbox: 'trusted_native',
                  tool_name: 'workspace.read_text',
                },
                available_decisions: ['allow_once', 'allow_session', 'deny'],
              }]
            : [],
        })
      }
      if (url === '/v1/approvals/approval-1:decide' && init?.method === 'POST') {
        return jsonResponse({
          id: 'approval-1', tenantId: 'tenant-1', runId: 'run-1', version: 4,
          status: 'approved', createdAt: '2026-08-02T04:00:00Z',
        })
      }
      return new Response(null, { status: 404 })
    }))
    const wrapper = mount(ApprovalInbox)
    await flushPromises()

    expect(wrapper.text()).toContain('参数、Agent 版本和 Tool 策略完全一致')
    await wrapper.get('[data-testid="approval-allow-session"]').trigger('click')
    await flushPromises()

    const decision = requests.find(request => request.url.endsWith(':decide'))
    expect(JSON.parse(String(decision?.init?.body))).toEqual({
      version: 3,
      decision: 'allow_session',
    })
    expect(wrapper.text()).toContain('本会话内相同 Tool 参数与策略')
  })

  it('refreshes authoritative state when another reviewer wins the version race', async () => {
    let listRequests = 0
    vi.stubGlobal('fetch', vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input)
      if (url === '/v1/approvals?status=pending&limit=50') {
        listRequests += 1
        return jsonResponse({ items: listRequests === 1 ? [pendingApproval] : [] })
      }
      if (url === '/v1/approvals/approval-1:decide' && init?.method === 'POST') {
        return jsonResponse({
          type: 'urn:agent-runtime:problem:approval',
          title: 'Approval is stale or no longer pending',
          status: 409,
        }, 409)
      }
      return new Response(null, { status: 404 })
    }))
    const wrapper = mount(ApprovalInbox)
    await flushPromises()

    await wrapper.get('[data-testid="approval-deny"]').trigger('click')
    await flushPromises()

    expect(listRequests).toBe(2)
    expect(wrapper.get('[role="status"]').text()).toContain('审批已被其他人处理')
    expect(wrapper.text()).toContain('当前没有待审批项')
  })
})

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': status >= 400 ? 'application/problem+json' : 'application/json' },
  })
}
