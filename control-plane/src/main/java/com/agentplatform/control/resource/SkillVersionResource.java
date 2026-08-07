package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.List;
import java.util.UUID;

public record SkillVersionResource(
    UUID id,
    UUID skillId,
    UUID applicationId,
    String name,
    String semanticVersion,
    String description,
    String instructions,
    List<String> toolNames,
    List<String> supportedPlatforms,
    String minRuntimeVersion,
    String artifactDigest,
    String signingKeyId,
    String signature,
    Instant createdAt) {
  public SkillVersionResource {
    toolNames = List.copyOf(toolNames);
    supportedPlatforms = List.copyOf(supportedPlatforms);
  }
}
