<script setup lang="ts">
import { computed, reactive, watch } from 'vue'
import ProviderCandidateEditor from './ProviderCandidateEditor.vue'
import SkillDraftEditor from './SkillDraftEditor.vue'
import type {
  ModelProviderDraft,
  ProjectOption,
  RuntimeSetupDraft,
  SkillDraft,
} from '../../types/resources'

const props = defineProps<{
  applicationName: string
  projects: readonly ProjectOption[]
  loading: boolean
  submitting: boolean
  completedSteps: number
  error: string | null
}>()

const emit = defineEmits<{
  submit: [draft: RuntimeSetupDraft]
}>()

interface EditableProvider extends ModelProviderDraft {
  key: number
}

let providerSequence = 1

function emptyProvider(): EditableProvider {
  return {
    key: providerSequence++,
    name: '',
    protocol: 'openai_compatible',
    endpoint: '',
    model: '',
    apiKey: '',
  }
}

const form = reactive({
  projectId: '',
  workspaceName: '开发工作区',
  agentName: 'Runtime Agent',
  instructions: '先检查工作区证据，再执行任务并报告可验证的结果。',
  skill: {
    name: 'workspace-review',
    semanticVersion: '1.0.0',
    description: '按受限证据审查工作区内容',
    instructions: '先读取相关文件，引用可验证证据后再给出结论。',
    toolNames: ['workspace.read_text'],
    supportedPlatforms: ['darwin-arm64', 'linux-arm64', 'linux-x86_64'],
    minRuntimeVersion: '0.1.0',
  } satisfies SkillDraft,
  modelPolicyName: '默认模型策略',
  providers: [emptyProvider()],
  sessionTitle: '新会话',
})

watch(
  () => props.projects,
  (projects) => {
    if (!projects.some(project => project.id === form.projectId)) {
      form.projectId = projects[0]?.id ?? ''
    }
  },
  { immediate: true },
)

const canSubmit = computed(() => Boolean(
  form.projectId
  && form.workspaceName.trim()
  && form.agentName.trim()
  && form.instructions.trim()
  && form.skill.name.trim()
  && form.skill.semanticVersion.trim()
  && form.skill.description.trim()
  && form.skill.instructions.trim()
  && form.modelPolicyName.trim()
  && form.providers.length > 0
  && form.providers.every(provider => provider.name.trim()
    && provider.endpoint.trim()
    && provider.model.trim()
    && provider.apiKey.trim())
  && form.sessionTitle.trim()
  && !props.loading
  && !props.submitting,
))

const totalSteps = computed(() => 6 + form.providers.length)
const progressLabel = computed(() => props.submitting
  ? `正在创建，第 ${Math.min(props.completedSteps + 1, totalSteps.value)} 步，共 ${totalSteps.value} 步`
  : `已完成 ${props.completedSteps}/${totalSteps.value}`)

function addProvider() {
  if (props.submitting || form.providers.length >= 8) return
  form.providers.push(emptyProvider())
}

function updateProvider(index: number, candidate: ModelProviderDraft) {
  const current = form.providers[index]
  if (!current || props.submitting) return
  form.providers[index] = { key: current.key, ...candidate }
}

function removeProvider(index: number) {
  if (props.submitting || index === 0 || form.providers.length === 1) return
  form.providers.splice(index, 1)
}

function updateSkill(skill: SkillDraft) {
  if (props.submitting) return
  form.skill = { ...skill, toolNames: [...skill.toolNames] }
}

function submit() {
  if (!canSubmit.value) return
  const draft: RuntimeSetupDraft = {
    projectId: form.projectId,
    workspaceName: form.workspaceName.trim(),
    agentName: form.agentName.trim(),
    instructions: form.instructions.trim(),
    skill: {
      ...form.skill,
      name: form.skill.name.trim(),
      semanticVersion: form.skill.semanticVersion.trim(),
      description: form.skill.description.trim(),
      instructions: form.skill.instructions.trim(),
      toolNames: [...form.skill.toolNames],
      supportedPlatforms: [...form.skill.supportedPlatforms],
    },
    modelPolicyName: form.modelPolicyName.trim(),
    routing: form.providers.length === 1 ? 'single_provider' : 'ordered_failover',
    providers: form.providers.map(provider => ({
      name: provider.name.trim(),
      protocol: provider.protocol,
      endpoint: provider.endpoint.trim(),
      model: provider.model.trim(),
      apiKey: provider.apiKey.trim(),
    })),
    sessionTitle: form.sessionTitle.trim(),
    delegatedScopes: form.skill.toolNames.includes('workspace.read_text')
      ? ['tool:workspace.read']
      : [],
  }
  emit('submit', draft)
  form.providers.forEach(provider => {
    provider.apiKey = ''
  })
}
</script>

<template>
  <section
    class="setup"
    aria-labelledby="runtime-setup-heading"
  >
    <div class="setup__heading">
      <div>
        <p class="setup__eyebrow">
          Runtime setup
        </p>
        <h2 id="runtime-setup-heading">
          配置新 Agent
        </h2>
      </div>
      <p>
        当前应用：<strong>{{ applicationName || '正在确认授权…' }}</strong>
      </p>
    </div>

    <div
      class="setup__progress"
      role="progressbar"
      aria-label="配置创建进度"
      aria-valuemin="0"
      :aria-valuemax="totalSteps"
      :aria-valuenow="completedSteps"
    >
      <span :style="{ width: `${(completedSteps / totalSteps) * 100}%` }" />
      <small>{{ progressLabel }}</small>
    </div>

    <form @submit.prevent="submit">
      <label class="field">
        <span>项目</span>
        <select
          v-model="form.projectId"
          name="projectId"
          :disabled="loading || submitting || projects.length === 0"
          required
        >
          <option
            v-for="project in projects"
            :key="project.id"
            :value="project.id"
          >
            {{ project.name }}
          </option>
        </select>
      </label>

      <section
        class="setup__providers"
        aria-labelledby="provider-heading"
      >
        <div class="setup__providers-heading">
          <div>
            <h3 id="provider-heading">
              模型 Provider
            </h3>
            <p>按顺序调用；只有尚未产生输出的安全错误才会切换备用项。</p>
          </div>
          <button
            name="addProvider"
            type="button"
            class="setup__secondary-action"
            :disabled="submitting || form.providers.length >= 8"
            @click="addProvider"
          >
            添加备用 Provider
          </button>
        </div>
        <ProviderCandidateEditor
          v-for="(provider, index) in form.providers"
          :key="provider.key"
          :candidate="provider"
          :index="index"
          :disabled="submitting"
          :removable="index > 0"
          @update="updateProvider(index, $event)"
          @remove="removeProvider(index)"
        />
      </section>

      <div class="setup__grid">
        <label class="field">
          <span>工作区名称</span>
          <input
            v-model="form.workspaceName"
            name="workspaceName"
            maxlength="200"
            :disabled="submitting"
            required
          >
        </label>
        <label class="field">
          <span>Agent 名称</span>
          <input
            v-model="form.agentName"
            name="agentName"
            maxlength="200"
            :disabled="submitting"
            required
          >
        </label>
        <label class="field">
          <span>模型策略名称</span>
          <input
            v-model="form.modelPolicyName"
            name="modelPolicyName"
            maxlength="200"
            :disabled="submitting"
            required
          >
        </label>
        <label class="field">
          <span>会话标题</span>
          <input
            v-model="form.sessionTitle"
            name="sessionTitle"
            maxlength="200"
            :disabled="submitting"
            required
          >
        </label>
      </div>

      <label class="field">
        <span>Agent 固定指令</span>
        <textarea
          v-model="form.instructions"
          name="instructions"
          rows="4"
          maxlength="32000"
          :disabled="submitting"
          required
        />
      </label>

      <SkillDraftEditor
        :skill="form.skill"
        :disabled="submitting"
        @update="updateSkill"
      />

      <p
        v-if="projects.length === 0 && !loading"
        class="setup__notice"
        role="status"
      >
        当前应用没有可用项目，请由平台管理员先创建项目。
      </p>
      <p
        v-if="error"
        class="setup__notice setup__notice--error"
        role="alert"
      >
        {{ error }}
      </p>

      <div class="setup__actions">
        <p>系统将创建 {{ totalSteps }} 个独立资源；已成功的步骤不会因后续失败而删除。</p>
        <button
          type="submit"
          :disabled="!canSubmit"
        >
          {{ submitting ? '正在配置…' : '创建并启用' }}
        </button>
      </div>
    </form>
  </section>
</template>

<style scoped>
.setup { background: #eef6f8; border: 1px solid #c9dde3; border-radius: 20px; margin-bottom: 2rem; padding: 1.4rem; }
.setup__heading { align-items: end; display: flex; gap: 2rem; justify-content: space-between; margin-bottom: 1rem; }
.setup__heading h2 { color: #172536; font-size: 1.25rem; margin: .2rem 0 0; }
.setup__heading > p { color: #536879; font-size: .86rem; margin: 0; }
.setup__eyebrow { color: #167072; font: 700 .7rem/1.4 monospace; letter-spacing: .11em; margin: 0; text-transform: uppercase; }
.setup__progress { background: #d7e7eb; border-radius: 999px; height: 8px; margin-bottom: 1.5rem; position: relative; }
.setup__progress > span { background: #167072; border-radius: inherit; display: block; height: 100%; transition: width .2s ease; }
.setup__progress > small { color: #536879; position: absolute; right: 0; top: .65rem; }
.setup form { display: grid; gap: 1rem; }
.setup__grid { display: grid; gap: .8rem; grid-template-columns: repeat(2, minmax(0, 1fr)); }
.field { color: #415064; display: grid; font-size: .8rem; font-weight: 650; gap: .42rem; }
.field select, .field textarea, .field input { background: #fff; border: 1px solid #b8cdd4; border-radius: 10px; color: #172536; min-height: 44px; padding: .7rem .8rem; width: 100%; }
.field textarea { line-height: 1.5; resize: vertical; }
.field select:focus-visible, .field textarea:focus-visible, .field input:focus-visible { border-color: #167072; outline: 3px solid #bfe7e5; outline-offset: 1px; }
.permission { align-items: center; color: #415064; display: flex; font-size: .82rem; font-weight: 650; gap: .55rem; }
.permission input { height: 18px; width: 18px; }
.setup__providers { display: grid; gap: .9rem; }
.setup__providers-heading { align-items: end; display: flex; gap: 1rem; justify-content: space-between; }
.setup__providers-heading h3 { color: #24475b; font-size: 1rem; margin: 0; }
.setup__providers-heading p { color: #657787; font-size: .76rem; margin: .3rem 0 0; }
.setup .setup__secondary-action { background: #fff; border: 1px solid #8eb4bf; color: #146466; min-height: 40px; }
.setup__notice { background: #fff; border-radius: 10px; color: #486178; font-size: .82rem; margin: 0; padding: .8rem; }
.setup__notice--error { background: #fff0f1; color: #9c2e3c; }
.setup__actions { align-items: center; display: flex; gap: 1rem; justify-content: space-between; }
.setup__actions p { color: #657787; font-size: .78rem; margin: 0; }
.setup button { background: #146466; border: 0; border-radius: 10px; color: white; cursor: pointer; font-weight: 700; min-height: 44px; padding: .72rem 1.1rem; }
.setup button:disabled { cursor: not-allowed; opacity: .5; }
.setup button:focus-visible { outline: 3px solid #75cfca; outline-offset: 3px; }
@media (max-width: 720px) {
  .setup__heading, .setup__actions, .setup__providers-heading { align-items: stretch; flex-direction: column; }
  .setup__grid { grid-template-columns: 1fr; }
}
</style>
