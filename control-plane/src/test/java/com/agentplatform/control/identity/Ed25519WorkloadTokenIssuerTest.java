package com.agentplatform.control.identity;

import static org.assertj.core.api.Assertions.assertThat;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.security.KeyPairGenerator;
import java.security.Signature;
import java.time.Clock;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.Base64;
import java.util.UUID;
import java.util.stream.StreamSupport;
import org.junit.jupiter.api.Test;

class Ed25519WorkloadTokenIssuerTest {
  @Test
  void tokenIsSignedBoundedAndRedactedFromDiagnostics() throws Exception {
    var keyPair = KeyPairGenerator.getInstance("Ed25519").generateKeyPair();
    var now = Instant.parse("2026-08-01T00:00:00Z");
    var issuer = new Ed25519WorkloadTokenIssuer(
        keyPair.getPrivate(), new ObjectMapper(),
        Clock.fixed(now.plusMillis(7), ZoneOffset.UTC));
    var workerIncarnationId = UUID.randomUUID();
    var claims = new WorkloadIdentityClaims(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        workerIncarnationId, UUID.randomUUID(), now, now.plusSeconds(30));

    var token = issuer.issue(claims);

    assertThat(token.toString()).isEqualTo("WorkloadToken[REDACTED]");
    assertThat(token.value()).startsWith("v2.");
    var parts = token.value().split("\\.");
    var payload = new String(Base64.getUrlDecoder().decode(parts[1]));
    var json = new ObjectMapper().readTree(payload);
    assertThat(json.path("tenant_id").asText()).isEqualTo(claims.tenantId().toString());
    assertThat(json.path("model_policy_id").asText()).isEqualTo(claims.modelPolicyId().toString());
    assertThat(json.path("worker_incarnation_id").asText())
        .isEqualTo(workerIncarnationId.toString());
    assertThat(json.path("schema_version").asInt()).isEqualTo(2);
    assertThat(StreamSupport.stream(json.path("audiences").spliterator(), false)
        .map(node -> node.asText()).toList())
        .containsExactlyInAnyOrder("model-gateway", "checkpoint-gateway");
    assertThat(StreamSupport.stream(json.path("scopes").spliterator(), false)
        .map(node -> node.asText()).toList())
        .containsExactlyInAnyOrder("model.execute", "checkpoint.read", "checkpoint.write");
    assertThat(json.path("expires_at_unix_ms").asLong()).isEqualTo(now.plusSeconds(30).toEpochMilli());
    assertThat(json.path("issued_at_unix_ms").asLong()).isEqualTo(now.toEpochMilli());
    var verifier = Signature.getInstance("Ed25519");
    verifier.initVerify(keyPair.getPublic());
    verifier.update(("v2." + parts[1]).getBytes(java.nio.charset.StandardCharsets.UTF_8));
    assertThat(verifier.verify(Base64.getUrlDecoder().decode(parts[2]))).isTrue();
  }
}
