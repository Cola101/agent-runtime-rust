package com.agentplatform.control.approval;

import java.time.Instant;
import java.util.UUID;

public record Approval(
    UUID id,
    UUID tenantId,
    UUID runId,
    int version,
    ApprovalStatus status,
    Instant createdAt) {}
