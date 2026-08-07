<script setup lang="ts">
import RunStatusBadge from './RunStatusBadge.vue'
import RunSteeringPanel from './runs/RunSteeringPanel.vue'
import type { RunSummary } from '../types/runtime'

defineProps<{
  runs: readonly RunSummary[]
  steeringRunId?: string | null
  steeringSubmitting?: boolean
  steeringError?: string | null
  acceptedSteeringId?: string | null
}>()
const emit = defineEmits<{ steer: [request: { runId: string, input: string }] }>()

const tokenCount = new Intl.NumberFormat('en-US')
const dateTime = new Intl.DateTimeFormat('zh-CN', {
  month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit',
})
</script>

<template>
  <div
    v-if="runs.length === 0"
    class="empty"
  >
    <div
      class="empty__mark"
      aria-hidden="true"
    >
      R
    </div>
    <h2>还没有运行记录</h2>
    <p>创建第一个 Run 后，状态、审批与预算会显示在这里。</p>
  </div>
  <div
    v-else
    class="table-shell"
  >
    <table>
      <caption class="sr-only">
        当前租户的 Agent 运行记录
      </caption>
      <thead>
        <tr><th>Agent / Workspace</th><th>状态</th><th>预算</th><th>创建时间</th><th>运行控制</th></tr>
      </thead>
      <tbody>
        <tr
          v-for="run in runs"
          :key="run.id"
        >
          <td><strong>{{ run.agentName }}</strong><small>{{ run.workspaceName }}</small></td>
          <td><RunStatusBadge :status="run.status" /></td>
          <td><strong>{{ tokenCount.format(run.budget.maxTokens) }} tokens</strong><small>费用上限 ${{ (run.budget.maxCostCents / 100).toFixed(2) }}</small></td>
          <td><time :datetime="run.createdAt">{{ dateTime.format(new Date(run.createdAt)) }}</time></td>
          <td>
            <RunSteeringPanel
              v-if="run.status === 'running'"
              :submitting="steeringRunId === run.id && steeringSubmitting"
              :error="steeringRunId === run.id ? steeringError : null"
              :accepted-id="steeringRunId === run.id ? acceptedSteeringId : null"
              @submit="emit('steer', { runId: run.id, input: $event })"
            />
            <small v-else>仅运行中可调整</small>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.table-shell { border: 1px solid #dce4ec; border-radius: 18px; overflow: hidden; }
table { border-collapse: collapse; width: 100%; }
th { background: #f6f8fb; color: #637083; font-size: .72rem; letter-spacing: .07em; text-align: left; text-transform: uppercase; }
th, td { padding: 1rem 1.2rem; }
td { border-top: 1px solid #e7edf3; color: #2a3545; font-size: .9rem; }
td strong, td small { display: block; }
td small { color: #738095; margin-top: .25rem; }
.empty { border: 1px dashed #cad5e1; border-radius: 18px; padding: 4rem 2rem; text-align: center; }
.empty__mark { align-items: center; background: #ebf5ff; border-radius: 14px; color: #1269a8; display: inline-flex; font: 700 1.2rem/1 monospace; height: 3rem; justify-content: center; width: 3rem; }
.empty h2 { color: #1c2a3a; font-size: 1rem; margin: 1rem 0 .35rem; }
.empty p { color: #708096; font-size: .88rem; margin: 0; }
.sr-only { height: 1px; margin: -1px; overflow: hidden; position: absolute; width: 1px; clip: rect(0, 0, 0, 0); }
@media (max-width: 720px) {
  th:nth-child(3), td:nth-child(3), th:nth-child(4), td:nth-child(4) { display: none; }
  th, td { padding: .85rem; }
}
</style>
