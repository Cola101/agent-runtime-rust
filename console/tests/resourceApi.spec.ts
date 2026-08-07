import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  createAgent,
  createAgentVersion,
  createModelPolicy,
  createModelProvider,
  publishSkillVersion,
  createSession,
  createWorkspace,
  fetchResourceContext,
} from '../src/api/resourceApi'

describe('resourceApi', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('maps only the authorized Application and Project context', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({
      application_id: '22222222-2222-4222-8222-222222222222',
      application_name: 'Local Agent Runtime',
      projects: [{ id: '33333333-3333-4333-8333-333333333333', name: 'Native Development' }],
    }), { status: 200, headers: { 'Content-Type': 'application/json' } })))

    await expect(fetchResourceContext()).resolves.toEqual({
      applicationId: '22222222-2222-4222-8222-222222222222',
      applicationName: 'Local Agent Runtime',
      projects: [{ id: '33333333-3333-4333-8333-333333333333', name: 'Native Development' }],
    })
    expect(fetch).toHaveBeenCalledWith('/v1/console/resource-context', {
      headers: { Accept: 'application/json' },
    })
  })

  it('creates independently authorized resources with server-generated identities', async () => {
    const responses = [
      { id: 'workspace-1', project_id: 'project-1', name: 'Workspace', state: 'ready', created_at: '2026-08-02T00:00:00Z' },
      { id: 'agent-1', workspace_id: 'workspace-1', name: 'Agent', created_at: '2026-08-02T00:00:01Z' },
      { id: 'skill-version-1', name: 'workspace-review', semantic_version: '1.0.0', description: 'Review evidence', instructions: 'Read evidence.', tool_names: ['workspace.read_text'], supported_platforms: ['darwin-arm64'], min_runtime_version: '0.1.0', artifact_digest: 'a'.repeat(64), signing_key_id: 'skill-key', signature: 'A'.repeat(86), created_at: '2026-08-02T00:00:02Z' },
      { id: 'version-1', agent_id: 'agent-1', version: 1, instructions: 'Inspect evidence.', delegated_scopes: ['tool:workspace.read'], skill_version_ids: ['skill-version-1'], created_at: '2026-08-02T00:00:03Z' },
      { id: 'provider-1', name: 'Primary Provider', protocol: 'openai_responses', endpoint: 'https://api.example.test/v1/responses', model: 'reasoning-model', state: 'active', credential_status: 'configured', created_at: '2026-08-02T00:00:04Z' },
      { id: 'policy-1', workspace_id: 'workspace-1', name: 'Primary', routing: 'ordered_failover', provider_ids: ['provider-1'], created_at: '2026-08-02T00:00:04Z' },
      { id: 'session-1', workspace_id: 'workspace-1', title: 'Review', state: 'active', created_at: '2026-08-02T00:00:05Z' },
    ]
    const fetchMock = vi.fn()
    for (const body of responses) {
      fetchMock.mockResolvedValueOnce(new Response(JSON.stringify(body), {
        status: 201,
        headers: { 'Content-Type': 'application/json' },
      }))
    }
    vi.stubGlobal('fetch', fetchMock)

    await expect(createWorkspace({ projectId: 'project-1', name: 'Workspace' }))
      .resolves.toMatchObject({ id: 'workspace-1', projectId: 'project-1' })
    await expect(createAgent({ workspaceId: 'workspace-1', name: 'Agent' }))
      .resolves.toMatchObject({ id: 'agent-1', workspaceId: 'workspace-1' })
    await expect(publishSkillVersion({
      name: 'workspace-review',
      semanticVersion: '1.0.0',
      description: 'Review evidence',
      instructions: 'Read evidence.',
      toolNames: ['workspace.read_text'],
      supportedPlatforms: ['darwin-arm64'],
      minRuntimeVersion: '0.1.0',
    })).resolves.toMatchObject({ id: 'skill-version-1', artifactDigest: 'a'.repeat(64) })
    await expect(createAgentVersion('agent-1', {
      instructions: 'Inspect evidence.',
      delegatedScopes: ['tool:workspace.read'],
      skillVersionIds: ['skill-version-1'],
    })).resolves.toMatchObject({ id: 'version-1', version: 1 })
    await expect(createModelProvider({
      name: 'Primary Provider',
      protocol: 'openai_responses',
      endpoint: 'https://api.example.test/v1/responses',
      model: 'reasoning-model',
      apiKey: 'write-only-secret',
    })).resolves.toEqual({
      id: 'provider-1',
      name: 'Primary Provider',
      protocol: 'openai_responses',
      endpoint: 'https://api.example.test/v1/responses',
      model: 'reasoning-model',
      state: 'active',
      credentialStatus: 'configured',
      createdAt: '2026-08-02T00:00:04Z',
    })
    await expect(createModelPolicy({
      workspaceId: 'workspace-1', name: 'Primary', routing: 'ordered_failover',
      providerIds: ['provider-1'],
    })).resolves.toMatchObject({ id: 'policy-1', routing: 'ordered_failover' })
    await expect(createSession({ workspaceId: 'workspace-1', title: 'Review' }))
      .resolves.toMatchObject({ id: 'session-1', state: 'active' })

    expect(fetchMock.mock.calls.map(call => call[0])).toEqual([
      '/v1/workspaces',
      '/v1/agents',
      '/v1/skills:publish',
      '/v1/agents/agent-1/versions',
      '/v1/model-providers',
      '/v1/model-policies',
      '/v1/sessions',
    ])
    expect(JSON.parse(fetchMock.mock.calls[2]?.[1]?.body as string)).toEqual({
      name: 'workspace-review',
      semantic_version: '1.0.0',
      description: 'Review evidence',
      instructions: 'Read evidence.',
      tool_names: ['workspace.read_text'],
      supported_platforms: ['darwin-arm64'],
      min_runtime_version: '0.1.0',
    })
    expect(JSON.parse(fetchMock.mock.calls[3]?.[1]?.body as string)).toEqual({
      instructions: 'Inspect evidence.',
      delegated_scopes: ['tool:workspace.read'],
      skill_version_ids: ['skill-version-1'],
    })
    expect(JSON.parse(fetchMock.mock.calls[4]?.[1]?.body as string)).toEqual({
      name: 'Primary Provider',
      protocol: 'openai_responses',
      endpoint: 'https://api.example.test/v1/responses',
      model: 'reasoning-model',
      api_key: 'write-only-secret',
    })
    expect(JSON.parse(fetchMock.mock.calls[5]?.[1]?.body as string)).toEqual({
      workspace_id: 'workspace-1',
      name: 'Primary',
      routing: 'ordered_failover',
      provider_ids: ['provider-1'],
    })
  })
})
