package com.agentplatform.control.scheduler;

import java.util.List;
import java.util.UUID;

public record SkillSnapshot(
    int schemaVersion,
    UUID applicationId,
    UUID skillVersionId,
    String name,
    String semanticVersion,
    String description,
    String instructions,
    List<String> toolNames,
    List<String> supportedPlatforms,
    String minRuntimeVersion,
    String artifactDigest,
    String signingKeyId,
    String signature) {
  public SkillSnapshot {
    toolNames = List.copyOf(toolNames);
    supportedPlatforms = List.copyOf(supportedPlatforms);
  }
}
