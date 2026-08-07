package com.agentplatform.control.approval;

import java.util.List;
import java.util.UUID;

public interface ApprovalRepository {
  List<ApprovalSummary> findPending(UUID tenantId, UUID applicationId, int limit);

  Approval decide(UUID tenantId, UUID applicationId, DecideApprovalCommand command);
}
