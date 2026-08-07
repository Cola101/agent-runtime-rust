package com.agentplatform.control.resource;

public record SignedSkillArtifact(
    String artifactDigest,
    String signingKeyId,
    String signature) {}
