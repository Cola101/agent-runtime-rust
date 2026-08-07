package com.agentplatform.control.run;

import java.util.UUID;

public final class RunNotFound extends RuntimeException {
  public RunNotFound(UUID runId) {
    super("run " + runId + " was not found");
  }
}
