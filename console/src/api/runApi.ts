import type { CreateRunDraft, RunAccepted, RunSteeringAccepted, RunTarget } from '../types/runtime'

interface WireRunTarget {
  session_id: string
  workspace_id: string
  workspace_name: string
  agent_version_id: string
  agent_name: string
  agent_version: number
  model_policy_id: string
  model_policy_name: string
}

interface WireRunTargetList { items: WireRunTarget[] }
interface WireRunAccepted { run_id: string, events_url: string }
interface WireRunSteeringAccepted { run_id: string, steering_id: string, state: 'pending' | 'applied' }

interface CreateRunRequest extends CreateRunDraft {
  idempotencyKey: string
}

interface SteerRunRequest {
  runId: string
  input: string
  idempotencyKey: string
}

export class RunSteeringRateLimitError extends Error {
  readonly retryAfterSeconds: number

  constructor(retryAfterSeconds: number) {
    super(`Run steering is rate limited for ${retryAfterSeconds} seconds`)
    this.name = 'RunSteeringRateLimitError'
    this.retryAfterSeconds = retryAfterSeconds
  }
}

async function requireJson<T>(response: Response, operation: string): Promise<T> {
  if (!response.ok) throw new Error(`${operation} failed with ${response.status}`)
  return response.json() as Promise<T>
}

export async function fetchRunTargets(): Promise<RunTarget[]> {
  const response = await fetch('/v1/console/run-targets', {
    headers: { Accept: 'application/json' },
  })
  const body = await requireJson<WireRunTargetList>(response, 'Run target list')
  return body.items.map(target => ({
    sessionId: target.session_id,
    workspaceId: target.workspace_id,
    workspaceName: target.workspace_name,
    agentVersionId: target.agent_version_id,
    agentName: target.agent_name,
    agentVersion: target.agent_version,
    modelPolicyId: target.model_policy_id,
    modelPolicyName: target.model_policy_name,
  }))
}

export async function createRun(request: CreateRunRequest): Promise<RunAccepted> {
  const response = await fetch(`/v1/sessions/${encodeURIComponent(request.target.sessionId)}/runs`, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      'Idempotency-Key': request.idempotencyKey,
    },
    body: JSON.stringify({
      agent_version_id: request.target.agentVersionId,
      workspace_id: request.target.workspaceId,
      model_policy_id: request.target.modelPolicyId,
      input: request.input,
      budget: {
        max_tokens: request.budget.maxTokens,
        max_cost_cents: request.budget.maxCostCents,
        max_duration_seconds: request.budget.maxDurationSeconds,
      },
    }),
  })
  const body = await requireJson<WireRunAccepted>(response, 'Run creation')
  return { runId: body.run_id, eventsUrl: body.events_url }
}

export async function steerRun(request: SteerRunRequest): Promise<RunSteeringAccepted> {
  const response = await fetch(`/v1/runs/${encodeURIComponent(request.runId)}:steer`, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      'Idempotency-Key': request.idempotencyKey,
    },
    body: JSON.stringify({ input: request.input }),
  })
  if (response.status === 429) {
    const parsedRetryAfter = Number.parseInt(response.headers.get('Retry-After') ?? '', 10)
    const retryAfterSeconds = Number.isSafeInteger(parsedRetryAfter) && parsedRetryAfter > 0
      ? parsedRetryAfter
      : 1
    throw new RunSteeringRateLimitError(retryAfterSeconds)
  }
  const body = await requireJson<WireRunSteeringAccepted>(response, 'Run steering')
  return { runId: body.run_id, steeringId: body.steering_id, state: body.state }
}
