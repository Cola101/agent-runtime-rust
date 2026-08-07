package com.agentplatform.control.run;

import java.util.Optional;
import java.util.List;
import java.util.UUID;
import java.time.Instant;

public interface RunRepository {
  Optional<Run> findByIdempotencyKey(
      UUID tenantId, UUID applicationId, String idempotencyKey);

  /**
   * Inserts the Run only if the tenant is under its concurrency quota.
   *
   * <p>The check and the insert are one operation on purpose. Counting active
   * Runs and then inserting lets two requests both count under the limit and
   * both insert; the implementation takes the tenant's quota row for update
   * first, which is the same shape subagent admission uses on the parent Run.
   *
   * @throws TenantQuotaExceeded when admitting this Run would exceed the limit
   */
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
