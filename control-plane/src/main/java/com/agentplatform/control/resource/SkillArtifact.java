package com.agentplatform.control.resource;

import java.util.List;
import java.util.UUID;

public record SkillArtifact(
    int schemaVersion,
    UUID tenantId,
    UUID applicationId,
    UUID skillVersionId,
    String name,
    String semanticVersion,
    String description,
    String instructions,
    List<String> toolNames,
    List<String> supportedPlatforms,
    String minRuntimeVersion) {
  public SkillArtifact {
    toolNames = List.copyOf(toolNames);
    supportedPlatforms = List.copyOf(supportedPlatforms);
  }
}
