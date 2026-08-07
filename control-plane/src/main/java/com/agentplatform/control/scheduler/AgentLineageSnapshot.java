package com.agentplatform.control.scheduler;

import java.util.UUID;

public record AgentLineageSnapshot(
    UUID rootRunId,
    UUID parentRunId,
    UUID delegationId,
    int depth,
    String role) {

  public static AgentLineageSnapshot primary(UUID runId) {
    return new AgentLineageSnapshot(runId, null, null, 0, "primary");
  }
}
