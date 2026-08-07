package com.agentplatform.control.scheduler;

import java.time.Instant;
import java.util.UUID;

public record ExecutionAcceptedMessage(
    int schemaVersion,
    UUID messageId,
    UUID tenantId,
    UUID runId,
    UUID attemptId,
    UUID workerId,
    UUID workerIncarnationId,
    Instant acceptedAt) {

  public ExecutionAcceptedMessage {
    if (schemaVersion != 1 && schemaVersion != 2) {
      throw new IllegalArgumentException(
          "unsupported execution acceptance schema version " + schemaVersion);
    }
    if (attemptId == null || attemptId.equals(new UUID(0, 0))) {
      throw new IllegalArgumentException("execution attempt id must not be nil");
    }
    if (schemaVersion == 2
        && (workerIncarnationId == null || workerIncarnationId.equals(new UUID(0, 0)))) {
      throw new IllegalArgumentException(
          "v2 execution acceptance must identify one worker incarnation");
    }
  }

  public ExecutionAcceptedMessage(
      int schemaVersion,
      UUID messageId,
      UUID tenantId,
      UUID runId,
      UUID attemptId,
      UUID workerId,
      Instant acceptedAt) {
    this(schemaVersion, messageId, tenantId, runId, attemptId, workerId, workerId, acceptedAt);
  }
}
