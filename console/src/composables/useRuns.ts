import { onMounted, readonly, ref, shallowRef } from 'vue'
import type { RunSummary } from '../types/runtime'

interface WireRunSummary {
  id: string
  workspace_name: string
  agent_name: string
  status: RunSummary['status']
  created_at: string
  budget: {
    max_tokens: number
    max_cost_cents: number
    max_duration_seconds: number
  }
}

interface RunListResponse { items: WireRunSummary[] }

export function mapRunListResponse(response: RunListResponse): RunSummary[] {
  return response.items.map(run => ({
    id: run.id,
    workspaceName: run.workspace_name,
    agentName: run.agent_name,
    status: run.status,
    createdAt: run.created_at,
    budget: {
      maxTokens: run.budget.max_tokens,
      maxCostCents: run.budget.max_cost_cents,
      maxDurationSeconds: run.budget.max_duration_seconds,
    },
  }))
}

export function useRuns() {
  const runs = ref<RunSummary[]>([])
  const loading = shallowRef(true)
  const error = shallowRef<string | null>(null)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      const response = await fetch('/v1/runs', { headers: { Accept: 'application/json' } })
      if (!response.ok) throw new Error(`Run list failed with ${response.status}`)
      runs.value = mapRunListResponse((await response.json()) as RunListResponse)
    } catch {
      error.value = '暂时无法载入运行记录，请稍后重试。'
    } finally {
      loading.value = false
    }
  }

  onMounted(refresh)
  return { runs: readonly(runs), loading: readonly(loading), error: readonly(error), refresh }
}
