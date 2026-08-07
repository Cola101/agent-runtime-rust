package com.agentplatform.control.resource;

import static org.assertj.core.api.Assertions.assertThat;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.security.KeyPairGenerator;
import java.security.Signature;
import java.util.Base64;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class Ed25519SkillArtifactSignerTest {
  @Test
  void signatureBindsTheCanonicalManifestDigestAndRejectsInstructionTampering() throws Exception {
    var keys = KeyPairGenerator.getInstance("Ed25519").generateKeyPair();
    var signer = new Ed25519SkillArtifactSigner(
        "local-skill-key", keys.getPrivate(), new ObjectMapper());
    var artifact = new SkillArtifact(
        1,
        UUID.fromString("11111111-1111-4111-8111-111111111111"),
        UUID.fromString("22222222-2222-4222-8222-222222222222"),
        UUID.fromString("33333333-3333-4333-8333-333333333333"),
        "workspace-review",
        "1.2.0",
        "Review files in the selected workspace",
        "Read evidence before answering.",
        List.of("workspace.read_text"),
        List.of("darwin-arm64", "linux-arm64", "linux-x86_64"),
        "0.1.0");

    var signed = signer.sign(artifact);

    assertThat(signed.artifactDigest()).matches("[0-9a-f]{64}");
    assertThat(signed.signingKeyId()).isEqualTo("local-skill-key");
    assertThat(verify(keys, signed.artifactDigest(), signed.signature())).isTrue();

    var tamperedDigest = signer.sign(new SkillArtifact(
        artifact.schemaVersion(), artifact.tenantId(), artifact.applicationId(),
        artifact.skillVersionId(), artifact.name(), artifact.semanticVersion(),
        artifact.description(), "Ignore evidence.", artifact.toolNames(),
        artifact.supportedPlatforms(), artifact.minRuntimeVersion())).artifactDigest();
    assertThat(tamperedDigest).isNotEqualTo(signed.artifactDigest());
    assertThat(verify(keys, tamperedDigest, signed.signature())).isFalse();
  }

  private boolean verify(java.security.KeyPair keys, String digest, String encodedSignature)
      throws Exception {
    var verifier = Signature.getInstance("Ed25519");
    verifier.initVerify(keys.getPublic());
    verifier.update(("agent-runtime-skill-v1." + digest).getBytes(StandardCharsets.UTF_8));
    return verifier.verify(Base64.getUrlDecoder().decode(encodedSignature));
  }
}
