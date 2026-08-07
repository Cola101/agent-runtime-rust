package com.agentplatform.control.run;

import java.util.Optional;
import java.util.List;
import java.util.UUID;
import java.time.Instant;

public interface RunRepository {
  Optional<Run> findByIdempotencyKey(
      UUID tenantId, UUID applicationId, String idempotencyKey);

  Run save(UUID applicationId, Run run);

  RunStatus requestCancellation(UUID tenantId, UUID runId, Instant requestedAt);

  RunStatus requestCancellation(
      UUID tenantId, UUID applicationId, UUID runId, Instant requestedAt);

  RunSteeringResult requestSteering(
      UUID tenantId,
      UUID applicationId,
      UUID runId,
      String idempotencyKey,
      String input,
      Instant requestedAt);

  default List<RunSummary> findRecent(UUID tenantId, UUID applicationId, int limit) {
    return List.of();
  }
}
