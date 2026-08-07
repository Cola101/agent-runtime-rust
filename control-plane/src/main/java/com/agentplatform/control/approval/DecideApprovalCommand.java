package com.agentplatform.control.approval;

import java.time.Instant;
import java.util.UUID;

public record DecideApprovalCommand(
    UUID approvalId,
    int expectedVersion,
    ApprovalDecision decision,
    String reason,
    String decidedBy,
    Instant decidedAt) {

  public DecideApprovalCommand {
    if (approvalId == null || decision == null || decidedAt == null
        || decidedBy == null || decidedBy.isBlank()) {
      throw new IllegalArgumentException("approval decision identity must be complete");
    }
    if (expectedVersion < 1) {
      throw new IllegalArgumentException("approval version must be positive");
    }
    if (reason != null && reason.length() > 1000) {
      throw new IllegalArgumentException("approval reason exceeds 1000 characters");
    }
  }
}
