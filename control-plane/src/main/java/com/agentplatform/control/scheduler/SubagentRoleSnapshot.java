package com.agentplatform.control.scheduler;

import java.util.List;

public record SubagentRoleSnapshot(
    String name,
    String instructions,
    List<String> delegatedScopes) {

  public SubagentRoleSnapshot {
    if (name == null || name.isBlank() || "primary".equals(name)
        || instructions == null || instructions.isBlank()) {
      throw new IllegalArgumentException("subagent role snapshot must be complete");
    }
    delegatedScopes = List.copyOf(delegatedScopes);
  }
}
