<script setup lang="ts">
import { reactive, watch } from 'vue'
import type { SkillDraft } from '../../types/resources'

const props = defineProps<{
  skill: SkillDraft
  disabled: boolean
}>()

const emit = defineEmits<{
  update: [skill: SkillDraft]
}>()

const local = reactive<SkillDraft>({ ...props.skill, toolNames: [...props.skill.toolNames] })

watch(
  () => props.skill,
  skill => Object.assign(local, { ...skill, toolNames: [...skill.toolNames] }),
  { deep: true },
)

function textValue(event: Event): string {
  return (event.target as HTMLInputElement | HTMLTextAreaElement).value
}

function updateText(field: 'name' | 'semanticVersion' | 'description' | 'instructions', value: string) {
  local[field] = value
  emit('update', { ...local, toolNames: [...local.toolNames] })
}

function updateWorkspaceRead(event: Event) {
  local.toolNames = (event.target as HTMLInputElement).checked ? ['workspace.read_text'] : []
  emit('update', { ...local, toolNames: [...local.toolNames] })
}
</script>

<template>
  <fieldset class="skill">
    <legend>Agent Skill</legend>
    <p class="skill__help">
      Skill 固定任务方法；发布时由平台自动签名，不能借此获得额外权限。
    </p>
    <div class="skill__grid">
      <label class="field">
        <span>标识</span>
        <input
          name="skillName"
          :value="local.name"
          maxlength="120"
          pattern="[a-z0-9](?:[a-z0-9._\-]*[a-z0-9])?"
          :disabled="disabled"
          required
          @input="updateText('name', textValue($event))"
        >
      </label>
      <label class="field">
        <span>版本</span>
        <input
          name="skillVersion"
          :value="local.semanticVersion"
          maxlength="64"
          :disabled="disabled"
          required
          @input="updateText('semanticVersion', textValue($event))"
        >
      </label>
      <label class="field skill__wide">
        <span>用途说明</span>
        <input
          name="skillDescription"
          :value="local.description"
          maxlength="500"
          :disabled="disabled"
          required
          @input="updateText('description', textValue($event))"
        >
      </label>
    </div>
    <label class="field">
      <span>Skill 指令</span>
      <textarea
        name="skillInstructions"
        :value="local.instructions"
        rows="3"
        maxlength="32000"
        :disabled="disabled"
        required
        @input="updateText('instructions', textValue($event))"
      />
    </label>
    <label class="permission">
      <input
        name="skillWorkspaceRead"
        type="checkbox"
        :checked="local.toolNames.includes('workspace.read_text')"
        :disabled="disabled"
        @change="updateWorkspaceRead"
      >
      <span>此 Skill 可调用平台预装的“只读工作区”可信 Tool（仍需运行时审批）</span>
    </label>
  </fieldset>
</template>

<style scoped>
.skill { border: 1px solid #b8cdd4; border-radius: 14px; display: grid; gap: .9rem; margin: 0; padding: 1rem; }
.skill legend { color: #24475b; font-size: 1rem; font-weight: 750; padding: 0 .35rem; }
.skill__help { color: #657787; font-size: .76rem; margin: -.3rem 0 0; }
.skill__grid { display: grid; gap: .8rem; grid-template-columns: 2fr 1fr; }
.skill__wide { grid-column: 1 / -1; }
.field { color: #415064; display: grid; font-size: .8rem; font-weight: 650; gap: .42rem; }
.field textarea, .field input { background: #fff; border: 1px solid #b8cdd4; border-radius: 10px; color: #172536; min-height: 44px; padding: .7rem .8rem; width: 100%; }
.field textarea { line-height: 1.5; resize: vertical; }
.field textarea:focus-visible, .field input:focus-visible { border-color: #167072; outline: 3px solid #bfe7e5; outline-offset: 1px; }
.permission { align-items: center; color: #415064; display: flex; font-size: .82rem; font-weight: 650; gap: .55rem; }
.permission input { height: 18px; width: 18px; }
@media (max-width: 720px) { .skill__grid { grid-template-columns: 1fr; } .skill__wide { grid-column: auto; } }
</style>
