import { onMounted, readonly, ref, shallowRef } from 'vue'
import {
  ApprovalConflictError,
  decideApproval,
  fetchPendingApprovals,
} from '../api/approvalApi'
import type { ApprovalDecision, ApprovalSummary } from '../types/runtime'

export function useApprovals() {
  const approvals = ref<ApprovalSummary[]>([])
  const loading = shallowRef(true)
  const decidingId = shallowRef<string | null>(null)
  const error = shallowRef<string | null>(null)
  const notice = shallowRef<string | null>(null)

  async function load() {
    loading.value = true
    error.value = null
    try {
      approvals.value = await fetchPendingApprovals()
    } catch {
      approvals.value = []
      error.value = '暂时无法载入待审批项，请稍后重试。'
    } finally {
      loading.value = false
    }
  }

  async function refresh() {
    notice.value = null
    await load()
  }

  async function decide(approval: ApprovalSummary, decision: ApprovalDecision) {
    decidingId.value = approval.id
    error.value = null
    notice.value = null
    try {
      await decideApproval(approval.id, approval.version, decision)
      notice.value = decision === 'allow_once'
        ? '已允许本次 Tool 执行。'
        : decision === 'allow_session'
          ? '已允许本会话内相同 Tool 参数与策略。'
          : '已拒绝本次 Tool 执行。'
      await load()
    } catch (caught) {
      if (caught instanceof ApprovalConflictError) {
        await load()
        notice.value = '审批已被其他人处理，列表已刷新。'
      } else {
        error.value = '审批提交失败，请确认 Runtime 状态后重试。'
      }
    } finally {
      decidingId.value = null
    }
  }

  onMounted(load)
  return {
    approvals: readonly(approvals),
    loading: readonly(loading),
    decidingId: readonly(decidingId),
    error: readonly(error),
    notice: readonly(notice),
    refresh,
    decide,
  }
}
