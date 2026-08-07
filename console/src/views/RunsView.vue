<script setup lang="ts">
import { shallowRef } from 'vue'
import RunList from '../components/RunList.vue'
import RunComposer from '../components/runs/RunComposer.vue'
import ApprovalInbox from '../components/approvals/ApprovalInbox.vue'
import RuntimeSetupPanel from '../components/resources/RuntimeSetupPanel.vue'
import type { RuntimeSetupDraft } from '../types/resources'
import type { CreateRunDraft, RunTarget } from '../types/runtime'
import { RunSteeringRateLimitError, steerRun } from '../api/runApi'
import { useRunCreation } from '../composables/useRunCreation'
import { useRuns } from '../composables/useRuns'
import { useRuntimeSetup } from '../composables/useRuntimeSetup'

const { runs, loading, error, refresh } = useRuns()
const {
  targets,
  loadingTargets,
  submitting,
  error: creationError,
  accepted,
  loadTargets,
  submitRun,
} = useRunCreation()
const {
  context: setupContext,
  loading: setupLoading,
  submitting: setupSubmitting,
  completedSteps: setupCompletedSteps,
  error: setupError,
  provision: provisionRuntime,
} = useRuntimeSetup()
const configuredTarget = shallowRef<RunTarget | null>(null)
const steeringRunId = shallowRef<string | null>(null)
const steeringSubmitting = shallowRef(false)
const steeringError = shallowRef<string | null>(null)
const acceptedSteeringId = shallowRef<string | null>(null)

async function create(draft: CreateRunDraft) {
  const result = await submitRun(draft)
  if (result) await refresh()
}

async function configure(draft: RuntimeSetupDraft) {
  configuredTarget.value = null
  const result = await provisionRuntime(draft)
  if (!result) return
  configuredTarget.value = result.target
  await loadTargets()
}

async function steer(request: { runId: string, input: string }) {
  steeringRunId.value = request.runId
  steeringSubmitting.value = true
  steeringError.value = null
  acceptedSteeringId.value = null
  try {
    const acceptedSteering = await steerRun({
      ...request,
      idempotencyKey: crypto.randomUUID(),
    })
    acceptedSteeringId.value = acceptedSteering.steeringId
  } catch (error) {
    steeringError.value = error instanceof RunSteeringRateLimitError
      ? `操作太频繁，请在 ${error.retryAfterSeconds} 秒后重试。`
      : '暂时无法调整该 Run；它可能已进入审批、工具执行或终态。'
  } finally {
    steeringSubmitting.value = false
  }
}
</script>

<template>
  <main>
    <header class="page-heading">
      <div>
        <p class="eyebrow">
          Runtime operations
        </p><h1>运行中心</h1><p>跟踪 Agent 运行、审批与预算消耗。</p>
      </div>
      <button
        type="button"
        @click="refresh"
      >
        刷新
      </button>
    </header>
    <RuntimeSetupPanel
      :application-name="setupContext?.applicationName ?? ''"
      :projects="setupContext?.projects ?? []"
      :loading="setupLoading"
      :submitting="setupSubmitting"
      :completed-steps="setupCompletedSteps"
      :error="setupError"
      @submit="configure"
    />
    <p
      v-if="configuredTarget"
      class="notice notice--success"
      role="status"
    >
      已创建 {{ configuredTarget.agentName }} v{{ configuredTarget.agentVersion }}，可直接启动 Run。
    </p>
    <RunComposer
      :targets="targets"
      :submitting="submitting || loadingTargets"
      :error="creationError"
      @submit="create"
    />
    <p
      v-if="accepted"
      class="notice notice--success"
      role="status"
    >
      Run 已受理：<code>{{ accepted.runId }}</code>
    </p>
    <ApprovalInbox />
    <p
      v-if="loading"
      role="status"
      class="notice"
    >
      正在载入运行记录…
    </p>
    <div
      v-else-if="error"
      role="alert"
      class="notice notice--error"
    >
      {{ error }}
    </div>
    <RunList
      v-else
      :runs="runs"
      :steering-run-id="steeringRunId"
      :steering-submitting="steeringSubmitting"
      :steering-error="steeringError"
      :accepted-steering-id="acceptedSteeringId"
      @steer="steer"
    />
  </main>
</template>

<style scoped>
main { margin: 0 auto; max-width: 1100px; padding: 4.5rem 2rem; }
.page-heading { align-items: end; display: flex; justify-content: space-between; margin-bottom: 2rem; }
.eyebrow { color: #1a73ad; font: 700 .72rem/1.4 monospace; letter-spacing: .12em; margin: 0 0 .6rem; text-transform: uppercase; }
h1 { color: #172536; font-size: clamp(2rem, 4vw, 3.4rem); letter-spacing: -.045em; margin: 0; }
.page-heading p:last-child { color: #68778b; margin: .7rem 0 0; }
button { background: #123d5a; border: 0; border-radius: 10px; color: white; cursor: pointer; font-weight: 650; padding: .72rem 1rem; }
button:focus-visible { outline: 3px solid #78bce8; outline-offset: 3px; }
.notice { background: #f1f6fa; border-radius: 14px; color: #486178; padding: 1rem; }
.notice--error { background: #fff0f1; color: #9c2e3c; }
.notice--success { background: #e8f6ef; color: #13734a; }
@media (max-width: 620px) { main { padding: 2rem 1rem; } .page-heading { align-items: start; gap: 1.5rem; } }
</style>
