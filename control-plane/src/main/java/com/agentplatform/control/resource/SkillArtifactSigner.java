package com.agentplatform.control.resource;

@FunctionalInterface
public interface SkillArtifactSigner {
  SignedSkillArtifact sign(SkillArtifact artifact);
}
