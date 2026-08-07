package com.agentplatform.control.approval;

import java.util.UUID;

public final class ApprovalNotFound extends RuntimeException {
  public ApprovalNotFound(UUID approvalId) {
    super("approval " + approvalId + " was not found in the current tenant");
  }
}
