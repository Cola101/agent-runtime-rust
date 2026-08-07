package com.agentplatform.control.resource;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.PrivateKey;
import java.security.Signature;
import java.util.Base64;
import java.util.HexFormat;
import java.util.Objects;
import java.util.TreeMap;

public final class Ed25519SkillArtifactSigner implements SkillArtifactSigner {
  private static final String DOMAIN = "agent-runtime-skill-v1.";
  private final String signingKeyId;
  private final PrivateKey privateKey;
  private final ObjectMapper mapper;

  public Ed25519SkillArtifactSigner(
      String signingKeyId, PrivateKey privateKey, ObjectMapper mapper) {
    if (signingKeyId == null || signingKeyId.isBlank() || signingKeyId.length() > 128) {
      throw new IllegalArgumentException("skill signing key id is required");
    }
    this.signingKeyId = signingKeyId;
    this.privateKey = Objects.requireNonNull(privateKey);
    this.mapper = Objects.requireNonNull(mapper);
  }

  @Override
  public SignedSkillArtifact sign(SkillArtifact artifact) {
    Objects.requireNonNull(artifact);
    try {
      var digest = HexFormat.of().formatHex(
          MessageDigest.getInstance("SHA-256").digest(canonicalBytes(artifact)));
      var signer = Signature.getInstance("Ed25519");
      signer.initSign(privateKey);
      signer.update((DOMAIN + digest).getBytes(StandardCharsets.UTF_8));
      return new SignedSkillArtifact(
          digest,
          signingKeyId,
          Base64.getUrlEncoder().withoutPadding().encodeToString(signer.sign()));
    } catch (java.security.GeneralSecurityException | JsonProcessingException exception) {
      throw new IllegalStateException("skill artifact could not be signed", exception);
    }
  }

  private byte[] canonicalBytes(SkillArtifact artifact) throws JsonProcessingException {
    var canonical = new TreeMap<String, Object>();
    canonical.put("schema_version", artifact.schemaVersion());
    canonical.put("tenant_id", artifact.tenantId().toString());
    canonical.put("application_id", artifact.applicationId().toString());
    canonical.put("skill_version_id", artifact.skillVersionId().toString());
    canonical.put("name", artifact.name());
    canonical.put("semantic_version", artifact.semanticVersion());
    canonical.put("description", artifact.description());
    canonical.put("instructions", artifact.instructions());
    canonical.put("tool_names", artifact.toolNames());
    canonical.put("supported_platforms", artifact.supportedPlatforms());
    canonical.put("min_runtime_version", artifact.minRuntimeVersion());
    return mapper.writeValueAsBytes(canonical);
  }
}
