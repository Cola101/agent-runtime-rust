import { mount } from '@vue/test-utils'
import { describe, expect, it } from 'vitest'
import RunSteeringPanel from '../src/components/runs/RunSteeringPanel.vue'

describe('RunSteeringPanel', () => {
  it('submits a bounded instruction through its public event', async () => {
    const wrapper = mount(RunSteeringPanel, { props: { submitting: false } })

    await wrapper.get('textarea[name="steeringInput"]').setValue('改为检查租户隔离')
    await wrapper.get('form').trigger('submit')

    expect(wrapper.emitted('submit')).toEqual([['改为检查租户隔离']])
    expect(wrapper.text()).toContain('24 / 32,768 字节')
  })

  it('does not emit blank or oversized input', async () => {
    const wrapper = mount(RunSteeringPanel, { props: { submitting: false } })

    await wrapper.get('form').trigger('submit')
    await wrapper.get('textarea[name="steeringInput"]').setValue('界'.repeat(11_000))
    await wrapper.get('form').trigger('submit')

    expect(wrapper.emitted('submit')).toBeUndefined()
    expect(wrapper.get('[role="alert"]').text()).toContain('32,768')
  })
})
