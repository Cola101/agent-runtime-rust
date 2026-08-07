package com.agentplatform.control.resource;

import java.util.List;
import java.util.UUID;

public interface RuntimeResourceRepository {
  ResourceContext findContext(UUID tenantId, UUID applicationId);

  WorkspaceResource createWorkspace(
      UUID tenantId, UUID applicationId, UUID projectId, String name);

  AgentResource createAgent(
      UUID tenantId, UUID applicationId, UUID workspaceId, String name);

  AgentVersionResource createAgentVersion(
      UUID tenantId,
      UUID applicationId,
      UUID agentId,
      String instructions,
      List<String> delegatedScopes,
      List<UUID> skillVersionIds,
      List<SubagentRoleDefinition> subagentRoles,
      java.util.Map<String, String> toolApprovalPolicies);

  SkillVersionResource publishSkillVersion(
      SkillArtifact artifact, SignedSkillArtifact signedArtifact);

  ModelProviderResource createModelProvider(
      UUID tenantId,
      UUID applicationId,
      UUID providerId,
      String name,
      String protocol,
      String endpoint,
      String model,
      String credentialEnvelope);

  McpServerResource createMcpServer(
      UUID tenantId,
      UUID applicationId,
      UUID serverId,
      String name,
      String endpoint,
      String credentialEnvelope);

  ModelPolicyResource createModelPolicy(
      UUID tenantId,
      UUID applicationId,
      UUID workspaceId,
      String name,
      String routing,
      List<UUID> providerIds);

  SessionResource createSession(
      UUID tenantId, UUID applicationId, UUID workspaceId, String title);
}
