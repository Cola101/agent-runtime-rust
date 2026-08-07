import { mount } from '@vue/test-utils'
import { describe, expect, it } from 'vitest'
import SkillDraftEditor from '../src/components/resources/SkillDraftEditor.vue'

describe('SkillDraftEditor', () => {
  it('edits user-visible Skill metadata while keeping trust as an explicit Tool choice', async () => {
    const skill = {
      name: 'workspace-review',
      semanticVersion: '1.0.0',
      description: 'Review evidence',
      instructions: 'Read evidence before answering.',
      toolNames: ['workspace.read_text'],
      supportedPlatforms: ['darwin-arm64', 'linux-arm64', 'linux-x86_64'],
      minRuntimeVersion: '0.1.0',
    }
    const wrapper = mount(SkillDraftEditor, { props: { skill, disabled: false } })

    expect(wrapper.find('input[name="artifactDigest"]').exists()).toBe(false)
    expect(wrapper.find('input[name="signature"]').exists()).toBe(false)
    await wrapper.get('textarea[name="skillInstructions"]').setValue('Inspect bounded files.')
    await wrapper.get('input[name="skillWorkspaceRead"]').setValue(false)

    expect(wrapper.emitted('update')?.at(-1)).toEqual([{
      ...skill,
      instructions: 'Inspect bounded files.',
      toolNames: [],
    }])
  })
})
