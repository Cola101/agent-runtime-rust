export type RunStatus =
  | 'queued'
  | 'running'
  | 'waiting_approval'
  | 'suspended'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'timed_out'
  | 'indeterminate'

export interface RunBudget {
  maxTokens: number
  maxCostCents: number
  maxDurationSeconds: number
}

export interface RunSummary {
  id: string
  workspaceName: string
  agentName: string
  status: RunStatus
  createdAt: string
  budget: RunBudget
}

export interface RunTarget {
  sessionId: string
  workspaceId: string
  workspaceName: string
  agentVersionId: string
  agentName: string
  agentVersion: number
  modelPolicyId: string
  modelPolicyName: string
}

export interface CreateRunDraft {
  target: RunTarget
  input: string
  budget: RunBudget
}

export interface RunAccepted {
  runId: string
  eventsUrl: string
}

export interface RunSteeringAccepted {
  runId: string
  steeringId: string
  state: 'pending' | 'applied'
}

export type ApprovalDecision = 'allow_once' | 'allow_session' | 'deny'

export interface ApprovalPolicySnapshot {
  approval: 'ask'
  effect: ApprovalSummary['effect']
  implementation_digest: string
  required_scopes: readonly string[]
  sandbox: ApprovalSummary['sandbox']
  tool_name: string
}

export interface ApprovalSummary {
  id: string
  runId: string
  version: number
  status: 'pending'
  workspaceName: string
  agentName: string
  toolName: string
  toolCallId: string
  effect: 'pure' | 'idempotent' | 'non_idempotent' | 'unknown'
  sandbox: 'restricted_container' | 'kata' | 'trusted_native'
  bindingDigest: string
  arguments: unknown
  policyDigest: string | null
  sessionScopeDigest: string | null
  policySnapshot: ApprovalPolicySnapshot | null
  availableDecisions: readonly ApprovalDecision[]
  createdAt: string
}
