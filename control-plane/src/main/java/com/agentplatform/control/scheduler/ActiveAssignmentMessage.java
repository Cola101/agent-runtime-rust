package com.agentplatform.control.scheduler;

import java.util.UUID;

public record ActiveAssignmentMessage(
    UUID tenantId,
    UUID runId,
    UUID attemptId,
    UUID workspaceId,
    long ownerEpoch,
    UUID fencingToken) {

  public ActiveAssignmentMessage {
    if (tenantId == null || runId == null || attemptId == null || workspaceId == null
        || fencingToken == null || ownerEpoch < 1) {
      throw new IllegalArgumentException("active assignment identity must be complete");
    }
  }
}
