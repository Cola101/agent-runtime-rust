package com.agentplatform.control.identity;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.security.InvalidKeyException;
import java.security.PrivateKey;
import java.security.Signature;
import java.security.SignatureException;
import java.time.Clock;
import java.time.Duration;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;

public final class Ed25519WorkloadTokenIssuer implements WorkloadTokenIssuer {
  private static final String TOKEN_VERSION = "v2";
  private static final Duration MAX_LIFETIME = Duration.ofMinutes(5);
  private final PrivateKey privateKey;
  private final ObjectMapper objectMapper;
  private final Clock clock;

  public Ed25519WorkloadTokenIssuer(PrivateKey privateKey, ObjectMapper objectMapper, Clock clock) {
    this.privateKey = Objects.requireNonNull(privateKey);
    this.objectMapper = Objects.requireNonNull(objectMapper);
    this.clock = Objects.requireNonNull(clock);
  }

  @Override
  public WorkloadToken issue(WorkloadIdentityClaims claims) {
    var issuedAt = claims.issuedAt();
    if (issuedAt.isAfter(clock.instant())) {
      throw new IllegalArgumentException("workload token issuance time must not be in the future");
    }
    var lifetime = Duration.between(issuedAt, claims.expiresAt());
    if (lifetime.isZero() || lifetime.isNegative() || lifetime.compareTo(MAX_LIFETIME) > 0) {
      throw new IllegalArgumentException("workload token lifetime must be between 1ms and 5 minutes");
    }
    Map<String, Object> payload = new LinkedHashMap<>();
    var snapshotBound = !claims.modelPolicyDigest().isBlank();
    if (snapshotBound && !claims.modelPolicyDigest().matches("[0-9a-f]{64}")) {
      throw new IllegalArgumentException("model policy digest must be lowercase SHA-256 hex");
    }
    payload.put("schema_version", snapshotBound ? 3 : 2);
    payload.put("tenant_id", claims.tenantId().toString());
    payload.put("run_id", claims.runId().toString());
    payload.put("attempt_id", claims.attemptId().toString());
    payload.put("worker_id", claims.workerId().toString());
    payload.put("worker_incarnation_id", claims.workerIncarnationId().toString());
    payload.put("model_policy_id", claims.modelPolicyId().toString());
    if (snapshotBound) payload.put("model_policy_digest", claims.modelPolicyDigest());
    payload.put("audiences", List.of("model-gateway", "checkpoint-gateway"));
    payload.put("scopes", List.of("model.execute", "checkpoint.read", "checkpoint.write"));
    payload.put("issued_at_unix_ms", issuedAt.toEpochMilli());
    payload.put("expires_at_unix_ms", claims.expiresAt().toEpochMilli());
    try {
      var encodedPayload = Base64.getUrlEncoder().withoutPadding()
          .encodeToString(objectMapper.writeValueAsBytes(payload));
      var signingInput = TOKEN_VERSION + "." + encodedPayload;
      var signer = Signature.getInstance("Ed25519");
      signer.initSign(privateKey);
      signer.update(signingInput.getBytes(StandardCharsets.UTF_8));
      var encodedSignature = Base64.getUrlEncoder().withoutPadding()
          .encodeToString(signer.sign());
      return new WorkloadToken(signingInput + "." + encodedSignature);
    } catch (JsonProcessingException | java.security.NoSuchAlgorithmException
             | InvalidKeyException | SignatureException exception) {
      throw new IllegalStateException("could not issue workload identity", exception);
    }
  }
}
