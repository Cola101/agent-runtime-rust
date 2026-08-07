package com.agentplatform.control.run;

import java.util.Set;
import java.util.UUID;

public record RunSteeringResult(UUID runId, UUID steeringId, String state) {
  private static final Set<String> STATES = Set.of("pending", "applied", "rejected", "cancelled");

  public RunSteeringResult {
    if (runId == null || steeringId == null || !STATES.contains(state)) {
      throw new IllegalArgumentException("run steering result is invalid");
    }
  }
}
