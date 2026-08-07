package com.agentplatform.control.run;

import java.time.Clock;
import java.util.Objects;
import java.util.List;
import java.util.UUID;

public final class RunService {
  private final RunRepository repository;
  private final Clock clock;

  public RunService(RunRepository repository) {
    this(repository, Clock.systemUTC());
  }

  RunService(RunRepository repository, Clock clock) {
    this.repository = Objects.requireNonNull(repository);
    this.clock = Objects.requireNonNull(clock);
  }

  public Run create(
      UUID tenantId,
      UUID applicationId,
      String idempotencyKey,
      CreateRunCommand command) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(applicationId, "applicationId");
    Objects.requireNonNull(command, "command");
    if (idempotencyKey == null || idempotencyKey.isBlank() || idempotencyKey.length() > 128) {
      throw new IllegalArgumentException("idempotency key must contain 1-128 characters");
    }

    return repository.findByIdempotencyKey(tenantId, applicationId, idempotencyKey)
        .orElseGet(() -> repository.save(applicationId,
        new Run(
            UUID.randomUUID(),
            tenantId,
            command.sessionId(),
            command.agentVersionId(),
            command.workspaceId(),
            command.modelPolicyId(),
            idempotencyKey,
            command.input(),
            RunStatus.QUEUED,
            command.maxTokens(),
            command.maxCostCents(),
            command.maxDurationSeconds(),
            clock.instant())));
  }

  public List<RunSummary> recent(UUID tenantId, UUID applicationId, int limit) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(applicationId, "applicationId");
    if (limit < 1 || limit > 100) {
      throw new IllegalArgumentException("limit must be between 1 and 100");
    }
    return repository.findRecent(tenantId, applicationId, limit);
  }

  public RunCancellationResult cancel(UUID tenantId, UUID applicationId, UUID runId) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(applicationId, "applicationId");
    Objects.requireNonNull(runId, "runId");
    return new RunCancellationResult(
        runId, repository.requestCancellation(tenantId, applicationId, runId, clock.instant()));
  }

  public RunSteeringResult steer(
      UUID tenantId,
      UUID applicationId,
      UUID runId,
      String idempotencyKey,
      SteerRunCommand command) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(applicationId, "applicationId");
    Objects.requireNonNull(runId, "runId");
    Objects.requireNonNull(command, "command");
    if (idempotencyKey == null || idempotencyKey.isBlank() || idempotencyKey.length() > 128) {
      throw new InvalidRunSteering("idempotency key must contain 1-128 characters");
    }
    return repository.requestSteering(
        tenantId, applicationId, runId, idempotencyKey, command.input(), clock.instant());
  }
}
