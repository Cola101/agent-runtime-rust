import { afterEach, describe, expect, it, vi } from 'vitest'
import { createRun, fetchRunTargets, steerRun } from '../src/api/runApi'
import type { RunTarget } from '../src/types/runtime'

describe('runApi', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('maps authorized run targets without exposing tenant identifiers', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({
      items: [{
        session_id: '77777777-7777-4777-8777-777777777777',
        workspace_id: '44444444-4444-4444-8444-444444444444',
        workspace_name: 'Local Workspace',
        agent_version_id: '66666666-6666-4666-8666-666666666666',
        agent_name: 'Local Runtime Agent',
        agent_version: 1,
        model_policy_id: '88888888-8888-4888-8888-888888888888',
        model_policy_name: 'Native Model Gateway',
      }],
    }), { status: 200, headers: { 'Content-Type': 'application/json' } })))

    const targets = await fetchRunTargets()

    expect(targets).toEqual([{
      sessionId: '77777777-7777-4777-8777-777777777777',
      workspaceId: '44444444-4444-4444-8444-444444444444',
      workspaceName: 'Local Workspace',
      agentVersionId: '66666666-6666-4666-8666-666666666666',
      agentName: 'Local Runtime Agent',
      agentVersion: 1,
      modelPolicyId: '88888888-8888-4888-8888-888888888888',
      modelPolicyName: 'Native Model Gateway',
    }])
  })

  it('posts the selected target, explicit budget and idempotency key', async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      run_id: '99999999-9999-4999-8999-999999999999',
      events_url: '/v1/runs/99999999-9999-4999-8999-999999999999/events',
    }), { status: 202, headers: { 'Content-Type': 'application/json' } }))
    vi.stubGlobal('fetch', fetchMock)
    const target: RunTarget = {
      sessionId: 'session-1', workspaceId: 'workspace-1', workspaceName: 'Workspace',
      agentVersionId: 'agent-version-1', agentName: 'Agent', agentVersion: 1,
      modelPolicyId: 'policy-1', modelPolicyName: 'Policy',
    }

    const accepted = await createRun({
      target,
      input: 'Inspect the runtime',
      budget: { maxTokens: 4000, maxCostCents: 200, maxDurationSeconds: 600 },
      idempotencyKey: 'request-fixed-1',
    })

    expect(accepted.runId).toBe('99999999-9999-4999-8999-999999999999')
    const [url, request] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/v1/sessions/session-1/runs')
    expect(request.method).toBe('POST')
    expect(request.headers).toEqual({
      Accept: 'application/json',
      'Content-Type': 'application/json',
      'Idempotency-Key': 'request-fixed-1',
    })
    expect(JSON.parse(request.body as string)).toEqual({
      agent_version_id: 'agent-version-1',
      workspace_id: 'workspace-1',
      model_policy_id: 'policy-1',
      input: 'Inspect the runtime',
      budget: { max_tokens: 4000, max_cost_cents: 200, max_duration_seconds: 600 },
    })
  })

  it('steers the same run with a fresh idempotency key', async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      run_id: 'run-1',
      steering_id: 'steering-1',
      state: 'pending',
    }), { status: 202, headers: { 'Content-Type': 'application/json' } }))
    vi.stubGlobal('fetch', fetchMock)

    const accepted = await steerRun({
      runId: 'run-1',
      input: '先停止旧分析，改为检查安全边界',
      idempotencyKey: 'steer-fixed-1',
    })

    expect(accepted).toEqual({ runId: 'run-1', steeringId: 'steering-1', state: 'pending' })
    const [url, request] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/v1/runs/run-1:steer')
    expect(request).toMatchObject({
      method: 'POST',
      headers: {
        Accept: 'application/json',
        'Content-Type': 'application/json',
        'Idempotency-Key': 'steer-fixed-1',
      },
    })
    expect(JSON.parse(request.body as string)).toEqual({ input: '先停止旧分析，改为检查安全边界' })
  })

  it('preserves the server retry window when steering is rate limited', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({
      type: 'urn:agent-runtime:problem:run-steering-rate-limit',
      title: 'Run steering rate limit exceeded',
      status: 429,
      retry_after_seconds: 2,
    }), {
      status: 429,
      headers: { 'Content-Type': 'application/problem+json', 'Retry-After': '2' },
    })))

    await expect(steerRun({
      runId: 'run-1', input: '再次调整', idempotencyKey: 'steer-fixed-2',
    })).rejects.toMatchObject({
      name: 'RunSteeringRateLimitError',
      retryAfterSeconds: 2,
    })
  })
})
