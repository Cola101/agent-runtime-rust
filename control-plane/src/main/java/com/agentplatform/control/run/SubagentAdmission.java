package com.agentplatform.control.run;

import java.util.UUID;

public record SubagentAdmission(
    UUID childRunId,
    UUID rootRunId,
    UUID parentRunId,
    UUID delegationId,
    int depth,
    String role,
    long remainingTokens,
    long remainingCostCents,
    long remainingDurationSeconds) {}
