package com.agentplatform.control.run;

import java.util.UUID;

public final class RunAlreadyTerminal extends RuntimeException {
  public RunAlreadyTerminal(UUID runId, RunStatus status) {
    super("run " + runId + " is already terminal with status " + status.name().toLowerCase());
  }
}
