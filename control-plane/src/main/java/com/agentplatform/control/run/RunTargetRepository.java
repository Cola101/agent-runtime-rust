package com.agentplatform.control.run;

import java.util.List;
import java.util.UUID;

public interface RunTargetRepository {
  List<RunTarget> findAvailable(UUID tenantId, UUID applicationId, int limit);
}
