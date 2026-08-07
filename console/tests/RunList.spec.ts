import { mount } from '@vue/test-utils'
import { describe, expect, it } from 'vitest'
import RunList from '../src/components/RunList.vue'
import type { RunSummary } from '../src/types/runtime'

describe('RunList', () => {
  it('shows state, budget and workspace without exposing another tenant field', () => {
    const runs: RunSummary[] = [
      {
        id: '0191f761-0ef0-7000-8000-000000000001',
        workspaceName: 'release-workspace',
        agentName: 'Release analyst',
        status: 'waiting_approval',
        createdAt: '2026-07-31T00:00:00Z',
        budget: { maxTokens: 12000, maxCostCents: 500, maxDurationSeconds: 3600 },
      },
    ]

    const wrapper = mount(RunList, { props: { runs } })

    expect(wrapper.get('[data-testid="run-status"]').text()).toBe('等待审批')
    expect(wrapper.text()).toContain('release-workspace')
    expect(wrapper.text()).toContain('12,000 tokens')
    expect(wrapper.text()).not.toContain('tenant_id')
  })

  it('renders a useful empty state', () => {
    const wrapper = mount(RunList, { props: { runs: [] } })
    expect(wrapper.text()).toContain('还没有运行记录')
  })

  it('allows steering only while a run is actively running', async () => {
    const runs: RunSummary[] = [
      {
        id: 'run-running', workspaceName: 'workspace-a', agentName: 'Agent A',
        status: 'running', createdAt: '2026-07-31T00:00:00Z',
        budget: { maxTokens: 1000, maxCostCents: 100, maxDurationSeconds: 60 },
      },
      {
        id: 'run-done', workspaceName: 'workspace-b', agentName: 'Agent B',
        status: 'succeeded', createdAt: '2026-07-31T00:01:00Z',
        budget: { maxTokens: 1000, maxCostCents: 100, maxDurationSeconds: 60 },
      },
    ]
    const wrapper = mount(RunList, { props: { runs, steeringRunId: null } })

    expect(wrapper.findAll('textarea[name="steeringInput"]')).toHaveLength(1)
    await wrapper.get('textarea[name="steeringInput"]').setValue('切换任务')
    await wrapper.get('form').trigger('submit')

    expect(wrapper.emitted('steer')).toEqual([[{ runId: 'run-running', input: '切换任务' }]])
  })
})
