import { mount } from '@vue/test-utils'
import { describe, expect, it } from 'vitest'
import RunComposer from '../src/components/runs/RunComposer.vue'
import type { RunTarget } from '../src/types/runtime'

describe('RunComposer', () => {
  it('emits a typed run request after the operator selects a target and enters instructions', async () => {
    const targets: RunTarget[] = [{
      sessionId: 'session-1', workspaceId: 'workspace-1', workspaceName: 'Local Workspace',
      agentVersionId: 'agent-version-1', agentName: 'Local Runtime Agent', agentVersion: 1,
      modelPolicyId: 'policy-1', modelPolicyName: 'Native Model Gateway',
    }]
    const wrapper = mount(RunComposer, { props: { targets, submitting: false, error: null } })

    await wrapper.get('textarea[name="instructions"]').setValue('Inspect the native runtime')
    await wrapper.get('form').trigger('submit')

    expect(wrapper.emitted('submit')).toEqual([[{
      target: targets[0],
      input: 'Inspect the native runtime',
      budget: { maxTokens: 4000, maxCostCents: 200, maxDurationSeconds: 600 },
    }]])
  })

  it('prevents an empty request and explains when no authorized target exists', async () => {
    const wrapper = mount(RunComposer, { props: { targets: [], submitting: false, error: null } })

    expect(wrapper.text()).toContain('没有可运行的 Agent 配置')
    expect(wrapper.get('button[type="submit"]').attributes('disabled')).toBeDefined()
    await wrapper.get('form').trigger('submit')
    expect(wrapper.emitted('submit')).toBeUndefined()
  })
})
