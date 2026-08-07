package com.agentplatform.control.approval;

import java.time.Clock;
import java.util.List;
import java.util.Objects;
import java.util.UUID;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Service;

@Service
public class ApprovalService {
  private final ApprovalRepository approvals;
  private final Clock clock;

  @Autowired
  public ApprovalService(ApprovalRepository approvals) {
    this(approvals, Clock.systemUTC());
  }

  ApprovalService(ApprovalRepository approvals, Clock clock) {
    this.approvals = Objects.requireNonNull(approvals);
    this.clock = Objects.requireNonNull(clock);
  }

  public List<ApprovalSummary> pending(UUID tenantId, UUID applicationId, int limit) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(applicationId, "applicationId");
    if (limit < 1 || limit > 100) {
      throw new IllegalArgumentException("approval list limit must be between 1 and 100");
    }
    return approvals.findPending(tenantId, applicationId, limit);
  }

  public Approval decide(
      UUID tenantId,
      UUID applicationId,
      UUID approvalId,
      int expectedVersion,
      ApprovalDecision decision,
      String reason,
      String decidedBy) {
    return approvals.decide(tenantId, applicationId, new DecideApprovalCommand(
        approvalId, expectedVersion, decision, reason, decidedBy, clock.instant()));
  }
}
