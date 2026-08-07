package com.agentplatform.control.identity;

import java.time.Instant;
import java.util.Objects;
import java.util.UUID;

public record WorkloadIdentityClaims(
    UUID tenantId,
    UUID runId,
    UUID attemptId,
    UUID workerId,
    UUID workerIncarnationId,
    UUID modelPolicyId,
    String modelPolicyDigest,
    Instant issuedAt,
    Instant expiresAt) {
  public WorkloadIdentityClaims {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(runId, "runId");
    Objects.requireNonNull(attemptId, "attemptId");
    Objects.requireNonNull(workerId, "workerId");
    Objects.requireNonNull(workerIncarnationId, "workerIncarnationId");
    Objects.requireNonNull(modelPolicyId, "modelPolicyId");
    Objects.requireNonNull(modelPolicyDigest, "modelPolicyDigest");
    Objects.requireNonNull(issuedAt, "issuedAt");
    Objects.requireNonNull(expiresAt, "expiresAt");
  }

  public WorkloadIdentityClaims(
      UUID tenantId,
      UUID runId,
      UUID attemptId,
      UUID workerId,
      UUID workerIncarnationId,
      UUID modelPolicyId,
      Instant issuedAt,
      Instant expiresAt) {
    this(
        tenantId,
        runId,
        attemptId,
        workerId,
        workerIncarnationId,
        modelPolicyId,
        "",
        issuedAt,
        expiresAt);
  }
}
