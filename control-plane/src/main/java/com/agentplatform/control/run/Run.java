package com.agentplatform.control.run;

import java.time.Instant;
import java.util.UUID;

public record Run(
    UUID id,
    UUID tenantId,
    UUID sessionId,
    UUID agentVersionId,
    UUID workspaceId,
    UUID modelPolicyId,
    String idempotencyKey,
    String input,
    RunStatus status,
    long maxTokens,
    long maxCostCents,
    long maxDurationSeconds,
    Instant createdAt) {}
