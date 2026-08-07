package com.agentplatform.control.run;

import java.util.List;
import java.util.Objects;
import java.util.UUID;

public final class RunTargetService {
  private final RunTargetRepository repository;

  public RunTargetService(RunTargetRepository repository) {
    this.repository = Objects.requireNonNull(repository);
  }

  public List<RunTarget> available(UUID tenantId, UUID applicationId, int limit) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(applicationId, "applicationId");
    if (limit < 1 || limit > 100) {
      throw new IllegalArgumentException("limit must be between 1 and 100");
    }
    return repository.findAvailable(tenantId, applicationId, limit);
  }
}
