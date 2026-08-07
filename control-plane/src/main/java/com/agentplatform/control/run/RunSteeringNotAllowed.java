package com.agentplatform.control.run;

import java.util.UUID;

public final class RunSteeringNotAllowed extends RuntimeException {
  public RunSteeringNotAllowed(UUID runId) {
    super("run cannot be steered at its current durable boundary: " + runId);
  }
}
