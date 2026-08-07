import type { RunTarget } from './runtime'

export interface ProjectOption {
  id: string
  name: string
}

export interface ResourceContext {
  applicationId: string
  applicationName: string
  projects: ProjectOption[]
}

export interface WorkspaceResource {
  id: string
  projectId: string
  name: string
  state: string
  createdAt: string
}

export interface AgentResource {
  id: string
  workspaceId: string
  name: string
  createdAt: string
}

export interface AgentVersionResource {
  id: string
  agentId: string
  version: number
  instructions: string
  delegatedScopes: string[]
  skillVersionIds: string[]
  createdAt: string
}

export interface SkillDraft {
  name: string
  semanticVersion: string
  description: string
  instructions: string
  toolNames: string[]
  supportedPlatforms: string[]
  minRuntimeVersion: string
}

export interface SkillVersionResource extends SkillDraft {
  id: string
  artifactDigest: string
  signingKeyId: string
  signature: string
  createdAt: string
}

export type ProviderProtocol = 'openai_compatible' | 'openai_responses' | 'anthropic_messages'

export interface ModelProviderDraft {
  name: string
  protocol: ProviderProtocol
  endpoint: string
  model: string
  apiKey: string
}

export interface ModelProviderResource {
  id: string
  name: string
  protocol: ProviderProtocol
  endpoint: string
  model: string
  state: string
  credentialStatus: string
  createdAt: string
}

export interface ModelPolicyResource {
  id: string
  workspaceId: string
  name: string
  routing: 'single_provider' | 'ordered_failover'
  providerIds: string[]
  createdAt: string
}

export interface SessionResource {
  id: string
  workspaceId: string
  title: string | null
  state: string
  createdAt: string
}

export interface RuntimeSetupDraft {
  projectId: string
  workspaceName: string
  agentName: string
  instructions: string
  skill: SkillDraft
  modelPolicyName: string
  routing: 'single_provider' | 'ordered_failover'
  providers: ModelProviderDraft[]
  sessionTitle: string
  delegatedScopes: string[]
}

export interface RuntimeSetupResult {
  target: RunTarget
}
