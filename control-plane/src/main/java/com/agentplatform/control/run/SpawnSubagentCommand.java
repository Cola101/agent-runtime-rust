package com.agentplatform.control.run;

import java.util.UUID;

public record SpawnSubagentCommand(
    UUID delegationId,
    String role,
    String input,
    long maxTokens,
    long maxCostCents,
    long maxDurationSeconds) {

  public SpawnSubagentCommand {
    if (delegationId == null || delegationId.equals(new UUID(0, 0))) {
      throw new IllegalArgumentException("delegation id is required");
    }
    if (role == null || "primary".equals(role)
        || !role.matches("[a-z0-9](?:[a-z0-9._-]{0,78}[a-z0-9])?")) {
      throw new IllegalArgumentException("subagent role must be a portable non-primary identifier");
    }
    if (input == null || input.isBlank()) {
      throw new IllegalArgumentException("subagent input is required");
    }
    if (maxTokens <= 0 || maxCostCents <= 0 || maxDurationSeconds <= 0
        || maxDurationSeconds > 86_400) {
      throw new IllegalArgumentException("subagent budgets must be finite and positive");
    }
  }
}
