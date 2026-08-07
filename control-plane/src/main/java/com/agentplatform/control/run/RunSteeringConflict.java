package com.agentplatform.control.run;

import java.util.UUID;

public final class RunSteeringConflict extends RuntimeException {
  public RunSteeringConflict(UUID runId) {
    super("run steering idempotency key was reused for different input: " + runId);
  }
}
