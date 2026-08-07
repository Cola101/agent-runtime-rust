import { onMounted, readonly, ref, shallowRef } from 'vue'
import { createRun, fetchRunTargets } from '../api/runApi'
import type { CreateRunDraft, RunAccepted, RunTarget } from '../types/runtime'

export function useRunCreation() {
  const targets = ref<RunTarget[]>([])
  const loadingTargets = shallowRef(true)
  const submitting = shallowRef(false)
  const error = shallowRef<string | null>(null)
  const accepted = shallowRef<RunAccepted | null>(null)

  async function loadTargets() {
    loadingTargets.value = true
    error.value = null
    try {
      targets.value = await fetchRunTargets()
    } catch {
      targets.value = []
      error.value = '暂时无法载入当前应用的 Agent 配置。'
    } finally {
      loadingTargets.value = false
    }
  }

  async function submitRun(draft: CreateRunDraft): Promise<RunAccepted | null> {
    submitting.value = true
    error.value = null
    accepted.value = null
    try {
      const result = await createRun({
        ...draft,
        idempotencyKey: crypto.randomUUID(),
      })
      accepted.value = result
      return result
    } catch {
      error.value = 'Run 提交失败，请检查模型与 Runtime 状态后重试。'
      return null
    } finally {
      submitting.value = false
    }
  }

  onMounted(loadTargets)
  return {
    targets: readonly(targets),
    loadingTargets: readonly(loadingTargets),
    submitting: readonly(submitting),
    error: readonly(error),
    accepted: readonly(accepted),
    loadTargets,
    submitRun,
  }
}
