package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.List;
import java.util.UUID;

public record ModelPolicyResource(
    UUID id,
    UUID workspaceId,
    String name,
    String routing,
    List<UUID> providerIds,
    Instant createdAt) {
  public ModelPolicyResource(
      UUID id, UUID workspaceId, String name, String routing, Instant createdAt) {
    this(id, workspaceId, name, routing, List.of(), createdAt);
  }
}
