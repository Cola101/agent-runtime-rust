package com.agentplatform.control.scheduler;

import java.time.Instant;
import java.util.Set;
import java.util.UUID;

public record RunSteeringOutcomeMessage(
    int schemaVersion,
    UUID messageId,
    UUID steeringId,
    UUID tenantId,
    UUID runId,
    UUID attemptId,
    UUID workerId,
    UUID workerIncarnationId,
    String inputDigest,
    String outcome,
    String reason,
    Instant occurredAt) {
  private static final Set<String> REJECTION_REASONS = Set.of(
      "expired", "wrong_worker", "wrong_worker_incarnation", "unknown_attempt",
      "attempt_conflict", "lease_expired", "attempt_terminal", "conflicting_replay",
      "invalid_command", "worker_rejected");

  public RunSteeringOutcomeMessage {
    if (schemaVersion != 1) {
      throw new IllegalArgumentException("unsupported run steering outcome schema version");
    }
    if (isMissing(messageId) || isMissing(steeringId) || isMissing(tenantId)
        || isMissing(runId) || isMissing(attemptId) || isMissing(workerId)
        || isMissing(workerIncarnationId)) {
      throw new IllegalArgumentException("run steering outcome identity is invalid");
    }
    if (inputDigest == null || !inputDigest.matches("^[0-9a-f]{64}$")) {
      throw new IllegalArgumentException("run steering outcome digest is invalid");
    }
    if (!"rejected".equals(outcome) || !REJECTION_REASONS.contains(reason)) {
      throw new IllegalArgumentException("run steering outcome classification is invalid");
    }
    if (occurredAt == null) {
      throw new IllegalArgumentException("run steering outcome time is required");
    }
  }

  private static boolean isMissing(UUID value) {
    return value == null || value.equals(new UUID(0, 0));
  }
}
