import { describe, expect, it } from 'vitest'
import { mapRunListResponse } from '../src/composables/useRuns'

describe('mapRunListResponse', () => {
  it('maps the public snake-case contract into console view models', () => {
    const result = mapRunListResponse({
      items: [{
        id: 'run-1',
        workspace_name: 'Workspace A',
        agent_name: 'Release analyst',
        status: 'running',
        created_at: '2026-07-31T00:00:00Z',
        budget: { max_tokens: 12000, max_cost_cents: 500, max_duration_seconds: 3600 },
      }],
    })

    expect(result[0]?.workspaceName).toBe('Workspace A')
    expect(result[0]?.budget.maxTokens).toBe(12000)
  })
})
