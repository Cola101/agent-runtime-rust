import { mount } from '@vue/test-utils'
import { describe, expect, it } from 'vitest'
import RuntimeSetupPanel from '../src/components/resources/RuntimeSetupPanel.vue'

describe('RuntimeSetupPanel', () => {
  it('submits a complete typed configuration without asking for tenant or application ids', async () => {
    const wrapper = mount(RuntimeSetupPanel, {
      props: {
        applicationName: 'Local Agent Runtime',
        projects: [{ id: 'project-1', name: 'Native Development' }],
        loading: false,
        submitting: false,
        completedSteps: 0,
        error: null,
      },
    })

    expect(wrapper.text()).toContain('Local Agent Runtime')
    expect(wrapper.find('input[name="tenant_id"]').exists()).toBe(false)
    expect(wrapper.find('input[name="application_id"]').exists()).toBe(false)
    await wrapper.get('input[name="workspaceName"]').setValue('Release Workspace')
    await wrapper.get('input[name="agentName"]').setValue('Release Agent')
    await wrapper.get('textarea[name="instructions"]').setValue('Review evidence before conclusions.')
    await wrapper.get('input[name="skillName"]').setValue('workspace-review')
    await wrapper.get('input[name="skillVersion"]').setValue('1.0.0')
    await wrapper.get('input[name="skillDescription"]').setValue('Review workspace evidence')
    await wrapper.get('textarea[name="skillInstructions"]').setValue('Read files before answering.')
    await wrapper.get('input[name="providerName-0"]').setValue('Primary OpenAI')
    await wrapper.get('input[name="providerEndpoint-0"]').setValue('https://api.example.test/v1/responses')
    await wrapper.get('input[name="providerModel-0"]').setValue('reasoning-model')
    await wrapper.get('input[name="providerApiKey-0"]').setValue('primary-secret')
    await wrapper.get('button[name="addProvider"]').trigger('click')
    await wrapper.get('input[name="providerName-1"]').setValue('Fallback Anthropic')
    await wrapper.get('select[name="providerProtocol-1"]').setValue('anthropic_messages')
    await wrapper.get('input[name="providerEndpoint-1"]').setValue('https://anthropic.example.test/v1/messages')
    await wrapper.get('input[name="providerModel-1"]').setValue('fallback-model')
    await wrapper.get('input[name="providerApiKey-1"]').setValue('fallback-secret')
    await wrapper.get('input[name="modelPolicyName"]').setValue('Primary')
    await wrapper.get('input[name="sessionTitle"]').setValue('Release review')
    await wrapper.get('input[name="skillWorkspaceRead"]').setValue(true)
    await wrapper.get('form').trigger('submit')

    expect(wrapper.emitted('submit')?.[0]).toEqual([{
      projectId: 'project-1',
      workspaceName: 'Release Workspace',
      agentName: 'Release Agent',
      instructions: 'Review evidence before conclusions.',
      skill: {
        name: 'workspace-review',
        semanticVersion: '1.0.0',
        description: 'Review workspace evidence',
        instructions: 'Read files before answering.',
        toolNames: ['workspace.read_text'],
        supportedPlatforms: ['darwin-arm64', 'linux-arm64', 'linux-x86_64'],
        minRuntimeVersion: '0.1.0',
      },
      modelPolicyName: 'Primary',
      routing: 'ordered_failover',
      providers: [
        {
          name: 'Primary OpenAI',
          protocol: 'openai_compatible',
          endpoint: 'https://api.example.test/v1/responses',
          model: 'reasoning-model',
          apiKey: 'primary-secret',
        },
        {
          name: 'Fallback Anthropic',
          protocol: 'anthropic_messages',
          endpoint: 'https://anthropic.example.test/v1/messages',
          model: 'fallback-model',
          apiKey: 'fallback-secret',
        },
      ],
      sessionTitle: 'Release review',
      delegatedScopes: ['tool:workspace.read'],
    }])
    expect((wrapper.get('input[name="providerApiKey-0"]').element as HTMLInputElement).value).toBe('')
    expect((wrapper.get('input[name="providerApiKey-1"]').element as HTMLInputElement).value).toBe('')
  })

  it('reports stepwise progress and prevents duplicate submission', () => {
    const wrapper = mount(RuntimeSetupPanel, {
      props: {
        applicationName: 'Local Agent Runtime',
        projects: [{ id: 'project-1', name: 'Native Development' }],
        loading: false,
        submitting: true,
        completedSteps: 3,
        error: null,
      },
    })

    expect(wrapper.get('[role="progressbar"]').attributes('aria-valuenow')).toBe('3')
    expect(wrapper.get('[role="progressbar"]').attributes('aria-valuemax')).toBe('7')
    expect(wrapper.get('button[type="submit"]').attributes()).toHaveProperty('disabled')
  })
})
