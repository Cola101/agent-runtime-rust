package com.agentplatform.control.scheduler;

import com.agentplatform.control.identity.WorkloadToken;
import java.time.Instant;
import java.util.List;
import java.util.UUID;

public record RunExecutionCommand(
    int schemaVersion,
    UUID messageId,
    UUID tenantId,
    UUID runId,
    UUID sessionId,
    UUID workspaceId,
    UUID agentVersionId,
    UUID modelPolicyId,
    UUID attemptId,
    UUID workerId,
    UUID workerIncarnationId,
    long ownerEpoch,
    UUID fencingToken,
    Instant issuedAt,
    Instant leaseExpiresAt,
    WorkloadToken workloadToken,
    List<String> delegatedScopes,
    String agentInstructions,
    String modelPolicySnapshotBase64,
    String modelPolicyDigest,
    List<SkillSnapshot> skillSnapshots,
    AgentLineageSnapshot lineage,
    List<SubagentRoleSnapshot> subagentRoles,
    /**
     * Per-Tool approval policy the tenant configured on the AgentVersion.
     *
     * <p>Empty means every Tool asks, which is what a subagent always gets: a
     * role-scoped exemption is a second decision nobody has made, so a child
     * never inherits one.
     */
    java.util.Map<String, String> toolApprovalPolicies,
    /**
     * Federated MCP servers this Run may reach.
     *
     * <p>Only servers whose {@code tool:mcp:<name>} scope is in
     * {@code delegatedScopes} are ever put here, and for a subagent that is the
     * role's scopes rather than the AgentVersion's -- a child does not reach a
     * server its role never delegated.
     */
    List<McpServerSnapshot> mcpServers,
    String input,
    long maxTokens,
    long maxCostCents,
    long maxDurationSeconds) {

  public RunExecutionCommand(
      int schemaVersion,
      UUID messageId,
      UUID tenantId,
      UUID runId,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID modelPolicyId,
      UUID attemptId,
      UUID workerId,
      UUID workerIncarnationId,
      long ownerEpoch,
      UUID fencingToken,
      Instant issuedAt,
      Instant leaseExpiresAt,
      WorkloadToken workloadToken,
      List<String> delegatedScopes,
      String agentInstructions,
      String input,
      long maxTokens,
      long maxCostCents,
      long maxDurationSeconds) {
    this(schemaVersion, messageId, tenantId, runId, sessionId, workspaceId, agentVersionId,
        modelPolicyId, attemptId, workerId, workerIncarnationId, ownerEpoch, fencingToken, issuedAt,
        leaseExpiresAt, workloadToken, delegatedScopes, agentInstructions, "", "", List.of(),
        AgentLineageSnapshot.primary(runId), List.of(),
        java.util.Map.of(), List.of(), input, maxTokens, maxCostCents,
        maxDurationSeconds);
  }

  public RunExecutionCommand(
      int schemaVersion,
      UUID messageId,
      UUID tenantId,
      UUID runId,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID modelPolicyId,
      UUID attemptId,
      UUID workerId,
      long ownerEpoch,
      UUID fencingToken,
      Instant issuedAt,
      Instant leaseExpiresAt,
      WorkloadToken workloadToken,
      List<String> delegatedScopes,
      String agentInstructions,
      String input,
      long maxTokens,
      long maxCostCents,
      long maxDurationSeconds) {
    this(schemaVersion, messageId, tenantId, runId, sessionId, workspaceId, agentVersionId,
        modelPolicyId, attemptId, workerId, workerId, ownerEpoch, fencingToken, issuedAt,
        leaseExpiresAt, workloadToken, delegatedScopes, agentInstructions, "", "", List.of(),
        AgentLineageSnapshot.primary(runId), List.of(),
        java.util.Map.of(), List.of(), input, maxTokens, maxCostCents,
        maxDurationSeconds);
  }

  public RunExecutionCommand {
    delegatedScopes = List.copyOf(delegatedScopes);
    skillSnapshots = List.copyOf(skillSnapshots);
    subagentRoles = List.copyOf(subagentRoles);
    mcpServers = mcpServers == null ? List.of() : List.copyOf(mcpServers);
    if (lineage == null) {
      throw new IllegalArgumentException("agent lineage is required");
    }
  }
}
