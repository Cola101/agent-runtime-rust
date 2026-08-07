package com.agentplatform.control.run;

import java.util.UUID;

public record CreateRunCommand(
    UUID sessionId,
    UUID agentVersionId,
    UUID workspaceId,
    UUID modelPolicyId,
    String input,
    long maxTokens,
    long maxCostCents,
    long maxDurationSeconds) {

  public CreateRunCommand {
    if (sessionId == null || agentVersionId == null || workspaceId == null || modelPolicyId == null) {
      throw new IllegalArgumentException("session, agent version, workspace and model policy are required");
    }
    if (input == null || input.isBlank()) {
      throw new IllegalArgumentException("input is required");
    }
    if (maxTokens <= 0 || maxCostCents <= 0 || maxDurationSeconds <= 0) {
      throw new IllegalArgumentException("run budgets must be finite and positive");
    }
  }
}
