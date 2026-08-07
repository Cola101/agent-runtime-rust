package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.List;
import java.util.UUID;

public record AgentVersionResource(
    UUID id,
    UUID agentId,
    int version,
    String instructions,
    List<String> delegatedScopes,
    List<UUID> skillVersionIds,
    List<SubagentRoleDefinition> subagentRoles,
    Instant createdAt) {
  public AgentVersionResource {
    delegatedScopes = List.copyOf(delegatedScopes);
    skillVersionIds = List.copyOf(skillVersionIds);
    subagentRoles = List.copyOf(subagentRoles);
  }

  public AgentVersionResource(
      UUID id,
      UUID agentId,
      int version,
      String instructions,
      List<String> delegatedScopes,
      List<UUID> skillVersionIds,
      Instant createdAt) {
    this(id, agentId, version, instructions, delegatedScopes, skillVersionIds, List.of(), createdAt);
  }

  public AgentVersionResource(
      UUID id,
      UUID agentId,
      int version,
      String instructions,
      List<String> delegatedScopes,
      Instant createdAt) {
    this(id, agentId, version, instructions, delegatedScopes, List.of(), List.of(), createdAt);
  }
}
