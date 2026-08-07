package com.agentplatform.control.run;

import static org.assertj.core.api.Assertions.assertThat;

import java.util.HashMap;
import java.util.Map;
import java.util.Optional;
import java.util.UUID;
import java.time.Instant;
import org.junit.jupiter.api.Test;

class RunServiceTest {

  @Test
  void sameTenantAndIdempotencyKeyReturnsTheOriginalRun() {
    var repository = new InMemoryRunRepository();
    var service = new RunService(repository);
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var request = new CreateRunCommand(
        sessionId, UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), "hello", 1000, 100, 60);

    var first = service.create(tenantId, applicationId, "request-42", request);
    var duplicate = service.create(tenantId, applicationId, "request-42", request);

    assertThat(duplicate).isEqualTo(first);
    assertThat(repository.count()).isOne();
  }

  @Test
  void idempotencyKeysAreIsolatedByTenant() {
    var repository = new InMemoryRunRepository();
    var service = new RunService(repository);
    var request = new CreateRunCommand(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "hello", 1000, 100, 60);

    var tenantARun = service.create(
        UUID.randomUUID(), UUID.randomUUID(), "same-key", request);
    var tenantBRun = service.create(
        UUID.randomUUID(), UUID.randomUUID(), "same-key", request);

    assertThat(tenantBRun.id()).isNotEqualTo(tenantARun.id());
    assertThat(repository.count()).isEqualTo(2);
  }

  @Test
  void cancellationUsesTenantScopedRepositoryAndReturnsCurrentPublicStatus() {
    var repository = new InMemoryRunRepository();
    var service = new RunService(repository);
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    repository.cancellationStatus = RunStatus.RUNNING;

    var result = service.cancel(tenantId, applicationId, runId);

    assertThat(result).isEqualTo(new RunCancellationResult(runId, RunStatus.RUNNING));
    assertThat(repository.cancelledTenantId).isEqualTo(tenantId);
    assertThat(repository.cancelledApplicationId).isEqualTo(applicationId);
    assertThat(repository.cancelledRunId).isEqualTo(runId);
  }

  @Test
  void steeringUsesTenantApplicationRunAndIdempotencyBoundaries() {
    var repository = new InMemoryRunRepository();
    var service = new RunService(repository);
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var steeringId = UUID.randomUUID();
    repository.steeringResult = new RunSteeringResult(runId, steeringId, "pending");

    var result = service.steer(
        tenantId, applicationId, runId, "steer-1", new SteerRunCommand("new direction"));

    assertThat(result).isEqualTo(repository.steeringResult);
    assertThat(repository.steeringTenantId).isEqualTo(tenantId);
    assertThat(repository.steeringApplicationId).isEqualTo(applicationId);
    assertThat(repository.steeringRunId).isEqualTo(runId);
    assertThat(repository.steeringIdempotencyKey).isEqualTo("steer-1");
    assertThat(repository.steeringInput).isEqualTo("new direction");
  }

  private static final class InMemoryRunRepository implements RunRepository {
    private final Map<String, Run> runs = new HashMap<>();
    private UUID cancelledTenantId;
    private UUID cancelledApplicationId;
    private UUID cancelledRunId;
    private RunStatus cancellationStatus;
    private UUID steeringTenantId;
    private UUID steeringApplicationId;
    private UUID steeringRunId;
    private String steeringIdempotencyKey;
    private String steeringInput;
    private RunSteeringResult steeringResult;

    @Override
    public Optional<Run> findByIdempotencyKey(
        UUID tenantId, UUID applicationId, String idempotencyKey) {
      return Optional.ofNullable(runs.get(key(tenantId, applicationId, idempotencyKey)));
    }

    @Override
    public Run save(UUID applicationId, Run run) {
      runs.put(key(run.tenantId(), applicationId, run.idempotencyKey()), run);
      return run;
    }

    @Override
    public RunStatus requestCancellation(UUID tenantId, UUID runId, Instant requestedAt) {
      cancelledTenantId = tenantId;
      cancelledRunId = runId;
      return cancellationStatus;
    }

    @Override
    public RunStatus requestCancellation(
        UUID tenantId, UUID applicationId, UUID runId, Instant requestedAt) {
      cancelledTenantId = tenantId;
      cancelledApplicationId = applicationId;
      cancelledRunId = runId;
      return cancellationStatus;
    }

    @Override
    public RunSteeringResult requestSteering(
        UUID tenantId,
        UUID applicationId,
        UUID runId,
        String idempotencyKey,
        String input,
        Instant requestedAt) {
      steeringTenantId = tenantId;
      steeringApplicationId = applicationId;
      steeringRunId = runId;
      steeringIdempotencyKey = idempotencyKey;
      steeringInput = input;
      return steeringResult;
    }

    int count() {
      return runs.size();
    }

    private String key(UUID tenantId, UUID applicationId, String idempotencyKey) {
      return tenantId + ":" + applicationId + ":" + idempotencyKey;
    }
  }
}
