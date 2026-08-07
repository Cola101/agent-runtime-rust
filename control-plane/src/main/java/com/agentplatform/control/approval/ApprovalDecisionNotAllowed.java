package com.agentplatform.control.approval;

import java.util.UUID;

public class ApprovalDecisionNotAllowed extends RuntimeException {
  public ApprovalDecisionNotAllowed(UUID approvalId) {
    super("approval " + approvalId + " does not support a session-scoped grant");
  }
}
