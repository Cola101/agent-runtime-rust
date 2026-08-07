<script setup lang="ts">
import { computed, reactive, watch } from 'vue'
import type { CreateRunDraft, RunTarget } from '../../types/runtime'

const props = defineProps<{
  targets: readonly RunTarget[]
  submitting: boolean
  error: string | null
}>()

const emit = defineEmits<{
  submit: [draft: CreateRunDraft]
}>()

const form = reactive({
  targetKey: '',
  instructions: '',
  maxTokens: 4000,
  maxCostCents: 200,
  maxDurationSeconds: 600,
})

function targetKey(target: RunTarget) {
  return `${target.sessionId}:${target.agentVersionId}:${target.modelPolicyId}`
}

const selectedTarget = computed(() =>
  props.targets.find(target => targetKey(target) === form.targetKey),
)

const canSubmit = computed(() => Boolean(
  selectedTarget.value
  && form.instructions.trim()
  && form.maxTokens > 0
  && form.maxCostCents > 0
  && form.maxDurationSeconds > 0
  && form.maxDurationSeconds <= 86400
  && !props.submitting,
))

watch(
  () => props.targets,
  (targets) => {
    if (!selectedTarget.value && targets[0]) form.targetKey = targetKey(targets[0])
  },
  { immediate: true },
)

function submit() {
  if (!canSubmit.value || !selectedTarget.value) return
  emit('submit', {
    target: selectedTarget.value,
    input: form.instructions.trim(),
    budget: {
      maxTokens: form.maxTokens,
      maxCostCents: form.maxCostCents,
      maxDurationSeconds: form.maxDurationSeconds,
    },
  })
}
</script>

<template>
  <section
    class="composer"
    aria-labelledby="new-run-heading"
  >
    <div class="composer__heading">
      <div>
        <p class="composer__eyebrow">
          New run
        </p>
        <h2 id="new-run-heading">
          启动 Agent
        </h2>
      </div>
      <p>选择当前应用有权使用的 Agent，然后给出任务与硬预算。</p>
    </div>

    <form @submit.prevent="submit">
      <label class="field field--target">
        <span>Agent 与工作区</span>
        <select
          v-model="form.targetKey"
          name="target"
          :disabled="targets.length === 0 || submitting"
        >
          <option
            v-for="target in targets"
            :key="targetKey(target)"
            :value="targetKey(target)"
          >
            {{ target.agentName }} v{{ target.agentVersion }} · {{ target.workspaceName }} · {{ target.modelPolicyName }}
          </option>
        </select>
      </label>

      <label class="field field--instructions">
        <span>任务说明</span>
        <textarea
          v-model="form.instructions"
          name="instructions"
          rows="4"
          maxlength="200000"
          placeholder="例如：检查当前 Runtime 的健康状态并总结风险。"
          :disabled="submitting"
          required
        />
      </label>

      <div
        class="budget"
        aria-label="运行预算"
      >
        <label class="field">
          <span>最大 Token</span>
          <input
            v-model.number="form.maxTokens"
            name="maxTokens"
            type="number"
            min="1"
            step="1"
          >
        </label>
        <label class="field">
          <span>最大费用（美分）</span>
          <input
            v-model.number="form.maxCostCents"
            name="maxCostCents"
            type="number"
            min="1"
            step="1"
          >
        </label>
        <label class="field">
          <span>最长时间（秒）</span>
          <input
            v-model.number="form.maxDurationSeconds"
            name="maxDurationSeconds"
            type="number"
            min="1"
            max="86400"
            step="1"
          >
        </label>
      </div>

      <p
        v-if="targets.length === 0"
        class="composer__notice"
        role="status"
      >
        没有可运行的 Agent 配置，请先确认当前应用下已有 Workspace、Session、AgentVersion 与模型策略。
      </p>
      <p
        v-if="error"
        class="composer__notice composer__notice--error"
        role="alert"
      >
        {{ error }}
      </p>

      <div class="composer__actions">
        <p>提交后会立即进入持久化队列；关闭页面不会取消 Run。</p>
        <button
          type="submit"
          :disabled="!canSubmit"
        >
          {{ submitting ? '正在提交…' : '启动 Run' }}
        </button>
      </div>
    </form>
  </section>
</template>

<style scoped>
.composer { background: #fff; border: 1px solid #dce4ec; border-radius: 20px; box-shadow: 0 18px 45px rgb(35 60 80 / 7%); margin-bottom: 2rem; padding: 1.4rem; }
.composer__heading { align-items: end; display: flex; gap: 2rem; justify-content: space-between; margin-bottom: 1.2rem; }
.composer__heading h2 { color: #172536; font-size: 1.25rem; margin: .2rem 0 0; }
.composer__heading > p { color: #68778b; font-size: .86rem; margin: 0; max-width: 32rem; }
.composer__eyebrow { color: #1a73ad; font: 700 .7rem/1.4 monospace; letter-spacing: .11em; margin: 0; text-transform: uppercase; }
form { display: grid; gap: 1rem; }
.field { color: #415064; display: grid; font-size: .8rem; font-weight: 650; gap: .42rem; }
.field select, .field textarea, .field input { background: #fbfcfe; border: 1px solid #cbd6e2; border-radius: 10px; color: #172536; min-height: 44px; padding: .7rem .8rem; width: 100%; }
.field textarea { line-height: 1.5; resize: vertical; }
.field select:focus-visible, .field textarea:focus-visible, .field input:focus-visible { border-color: #267aaa; outline: 3px solid #cbeafb; outline-offset: 1px; }
.budget { display: grid; gap: .8rem; grid-template-columns: repeat(3, minmax(0, 1fr)); }
.composer__notice { background: #f1f6fa; border-radius: 10px; color: #486178; font-size: .82rem; margin: 0; padding: .8rem; }
.composer__notice--error { background: #fff0f1; color: #9c2e3c; }
.composer__actions { align-items: center; display: flex; gap: 1rem; justify-content: space-between; }
.composer__actions p { color: #778597; font-size: .78rem; margin: 0; }
button { background: #123d5a; border: 0; border-radius: 10px; color: white; cursor: pointer; font-weight: 700; min-height: 44px; padding: .72rem 1.1rem; }
button:disabled { cursor: not-allowed; opacity: .5; }
button:focus-visible { outline: 3px solid #78bce8; outline-offset: 3px; }
@media (max-width: 720px) {
  .composer__heading, .composer__actions { align-items: stretch; flex-direction: column; }
  .budget { grid-template-columns: 1fr; }
}
</style>
