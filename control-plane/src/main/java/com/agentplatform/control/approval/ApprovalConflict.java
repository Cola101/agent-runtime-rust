package com.agentplatform.control.approval;

import java.util.UUID;

public final class ApprovalConflict extends RuntimeException {
  public ApprovalConflict(UUID approvalId) {
    super("approval " + approvalId + " is stale or no longer pending");
  }
}
