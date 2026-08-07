import type {
  AgentResource,
  AgentVersionResource,
  ModelPolicyResource,
  ModelProviderResource,
  ProviderProtocol,
  ResourceContext,
  SessionResource,
  SkillDraft,
  SkillVersionResource,
  WorkspaceResource,
} from '../types/resources'

interface WireResourceContext {
  application_id: string
  application_name: string
  projects: Array<{ id: string, name: string }>
}

interface WireWorkspace {
  id: string
  project_id: string
  name: string
  state: string
  created_at: string
}

interface WireAgent {
  id: string
  workspace_id: string
  name: string
  created_at: string
}

interface WireAgentVersion {
  id: string
  agent_id: string
  version: number
  instructions: string
  delegated_scopes: string[]
  skill_version_ids: string[]
  created_at: string
}

interface WireSkillVersion {
  id: string
  name: string
  semantic_version: string
  description: string
  instructions: string
  tool_names: string[]
  supported_platforms: string[]
  min_runtime_version: string
  artifact_digest: string
  signing_key_id: string
  signature: string
  created_at: string
}

interface WireModelPolicy {
  id: string
  workspace_id: string
  name: string
  routing: 'single_provider' | 'ordered_failover'
  provider_ids: string[]
  created_at: string
}

interface WireModelProvider {
  id: string
  name: string
  protocol: ProviderProtocol
  endpoint: string
  model: string
  state: string
  credential_status: string
  created_at: string
}

interface WireSession {
  id: string
  workspace_id: string
  title: string | null
  state: string
  created_at: string
}

async function requireJson<T>(response: Response, operation: string): Promise<T> {
  if (!response.ok) throw new Error(`${operation} failed with ${response.status}`)
  return response.json() as Promise<T>
}

async function post<T>(path: string, body: unknown, operation: string): Promise<T> {
  return requireJson<T>(await fetch(path, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(body),
  }), operation)
}

export async function fetchResourceContext(): Promise<ResourceContext> {
  const body = await requireJson<WireResourceContext>(await fetch('/v1/console/resource-context', {
    headers: { Accept: 'application/json' },
  }), 'Resource context')
  return {
    applicationId: body.application_id,
    applicationName: body.application_name,
    projects: body.projects,
  }
}

export async function createWorkspace(request: {
  projectId: string
  name: string
}): Promise<WorkspaceResource> {
  const body = await post<WireWorkspace>('/v1/workspaces', {
    project_id: request.projectId,
    name: request.name,
  }, 'Workspace creation')
  return {
    id: body.id,
    projectId: body.project_id,
    name: body.name,
    state: body.state,
    createdAt: body.created_at,
  }
}

export async function createAgent(request: {
  workspaceId: string
  name: string
}): Promise<AgentResource> {
  const body = await post<WireAgent>('/v1/agents', {
    workspace_id: request.workspaceId,
    name: request.name,
  }, 'Agent creation')
  return {
    id: body.id,
    workspaceId: body.workspace_id,
    name: body.name,
    createdAt: body.created_at,
  }
}

export async function createAgentVersion(agentId: string, request: {
  instructions: string
  delegatedScopes: string[]
  skillVersionIds: string[]
}): Promise<AgentVersionResource> {
  const body = await post<WireAgentVersion>(
    `/v1/agents/${encodeURIComponent(agentId)}/versions`,
    {
      instructions: request.instructions,
      delegated_scopes: request.delegatedScopes,
      skill_version_ids: request.skillVersionIds,
    },
    'Agent version creation',
  )
  return {
    id: body.id,
    agentId: body.agent_id,
    version: body.version,
    instructions: body.instructions,
    delegatedScopes: body.delegated_scopes,
    skillVersionIds: body.skill_version_ids,
    createdAt: body.created_at,
  }
}

export async function publishSkillVersion(request: SkillDraft): Promise<SkillVersionResource> {
  const body = await post<WireSkillVersion>('/v1/skills:publish', {
    name: request.name,
    semantic_version: request.semanticVersion,
    description: request.description,
    instructions: request.instructions,
    tool_names: request.toolNames,
    supported_platforms: request.supportedPlatforms,
    min_runtime_version: request.minRuntimeVersion,
  }, 'Skill publication')
  return {
    id: body.id,
    name: body.name,
    semanticVersion: body.semantic_version,
    description: body.description,
    instructions: body.instructions,
    toolNames: body.tool_names,
    supportedPlatforms: body.supported_platforms,
    minRuntimeVersion: body.min_runtime_version,
    artifactDigest: body.artifact_digest,
    signingKeyId: body.signing_key_id,
    signature: body.signature,
    createdAt: body.created_at,
  }
}

export async function createModelPolicy(request: {
  workspaceId: string
  name: string
  routing: 'single_provider' | 'ordered_failover'
  providerIds: string[]
}): Promise<ModelPolicyResource> {
  const body = await post<WireModelPolicy>('/v1/model-policies', {
    workspace_id: request.workspaceId,
    name: request.name,
    routing: request.routing,
    provider_ids: request.providerIds,
  }, 'Model policy creation')
  return {
    id: body.id,
    workspaceId: body.workspace_id,
    name: body.name,
    routing: body.routing,
    providerIds: body.provider_ids,
    createdAt: body.created_at,
  }
}

export async function createModelProvider(request: {
  name: string
  protocol: ProviderProtocol
  endpoint: string
  model: string
  apiKey: string
}): Promise<ModelProviderResource> {
  const body = await post<WireModelProvider>('/v1/model-providers', {
    name: request.name,
    protocol: request.protocol,
    endpoint: request.endpoint,
    model: request.model,
    api_key: request.apiKey,
  }, 'Model provider creation')
  return {
    id: body.id,
    name: body.name,
    protocol: body.protocol,
    endpoint: body.endpoint,
    model: body.model,
    state: body.state,
    credentialStatus: body.credential_status,
    createdAt: body.created_at,
  }
}

export async function createSession(request: {
  workspaceId: string
  title: string
}): Promise<SessionResource> {
  const body = await post<WireSession>('/v1/sessions', {
    workspace_id: request.workspaceId,
    title: request.title,
  }, 'Session creation')
  return {
    id: body.id,
    workspaceId: body.workspace_id,
    title: body.title,
    state: body.state,
    createdAt: body.created_at,
  }
}
