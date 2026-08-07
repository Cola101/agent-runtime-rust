package com.agentplatform.control.run;

import java.util.UUID;

public final class SubagentParentNotFound extends RuntimeException {
  public SubagentParentNotFound(UUID parentRunId) {
    super("subagent parent Run not found: " + parentRunId);
  }
}
