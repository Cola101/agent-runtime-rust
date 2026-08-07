import type { ApprovalDecision, ApprovalSummary } from '../types/runtime'

interface WireApprovalSummary {
  id: string
  run_id: string
  version: number
  status: 'pending'
  workspace_name: string
  agent_name: string
  tool_name: string
  tool_call_id: string
  effect: ApprovalSummary['effect']
  sandbox: ApprovalSummary['sandbox']
  binding_digest: string
  arguments: unknown
  policy_digest?: string | null
  session_scope_digest?: string | null
  policy_snapshot?: ApprovalSummary['policySnapshot']
  available_decisions?: ApprovalDecision[]
  created_at: string
}

interface WireApprovalList { items: WireApprovalSummary[] }

export class ApprovalConflictError extends Error {
  constructor() {
    super('approval is stale or no longer pending')
    this.name = 'ApprovalConflictError'
  }
}

export async function fetchPendingApprovals(): Promise<ApprovalSummary[]> {
  const response = await fetch('/v1/approvals?status=pending&limit=50', {
    headers: { Accept: 'application/json' },
  })
  if (!response.ok) throw new Error(`Approval list failed with ${response.status}`)
  const body = await response.json() as WireApprovalList
  return body.items.map(item => ({
    id: item.id,
    runId: item.run_id,
    version: item.version,
    status: item.status,
    workspaceName: item.workspace_name,
    agentName: item.agent_name,
    toolName: item.tool_name,
    toolCallId: item.tool_call_id,
    effect: item.effect,
    sandbox: item.sandbox,
    bindingDigest: item.binding_digest,
    arguments: item.arguments,
    policyDigest: item.policy_digest ?? null,
    sessionScopeDigest: item.session_scope_digest ?? null,
    policySnapshot: item.policy_snapshot ?? null,
    availableDecisions: item.available_decisions ?? ['allow_once', 'deny'],
    createdAt: item.created_at,
  }))
}

export async function decideApproval(
  approvalId: string,
  version: number,
  decision: ApprovalDecision,
): Promise<void> {
  const response = await fetch(`/v1/approvals/${encodeURIComponent(approvalId)}:decide`, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({ version, decision }),
  })
  if (response.status === 409) throw new ApprovalConflictError()
  if (!response.ok) throw new Error(`Approval decision failed with ${response.status}`)
}
