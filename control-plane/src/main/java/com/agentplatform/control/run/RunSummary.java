package com.agentplatform.control.run;

import java.time.Instant;
import java.util.UUID;

public record RunSummary(
    UUID id,
    String workspaceName,
    String agentName,
    RunStatus status,
    long maxTokens,
    long maxCostCents,
    long maxDurationSeconds,
    Instant createdAt) {}
