package com.agentplatform.control.approval;

import com.fasterxml.jackson.databind.JsonNode;
import java.time.Instant;
import java.util.UUID;

public record ApprovalSummary(
    UUID id,
    UUID runId,
    int version,
    ApprovalStatus status,
    String workspaceName,
    String agentName,
    String toolName,
    String toolCallId,
    String effect,
    String sandbox,
    String bindingDigest,
    JsonNode arguments,
    Instant createdAt,
    String policyDigest,
    String sessionScopeDigest,
    JsonNode policySnapshot,
    boolean sessionGrantEligible) {
  public ApprovalSummary(
      UUID id,
      UUID runId,
      int version,
      ApprovalStatus status,
      String workspaceName,
      String agentName,
      String toolName,
      String toolCallId,
      String effect,
      String sandbox,
      String bindingDigest,
      JsonNode arguments,
      Instant createdAt) {
    this(
        id, runId, version, status, workspaceName, agentName, toolName, toolCallId,
        effect, sandbox, bindingDigest, arguments, createdAt, null, null, null, false);
  }
}
