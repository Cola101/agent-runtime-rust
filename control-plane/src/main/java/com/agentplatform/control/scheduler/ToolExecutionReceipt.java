package com.agentplatform.control.scheduler;

import java.util.UUID;

public record ToolExecutionReceipt(
    UUID tenantId,
    UUID runId,
    UUID attemptId,
    String toolCallId,
    String bindingDigest,
    String effect,
    String sandbox,
    String state,
    UUID requestedEventId,
    UUID startedEventId,
    UUID resultEventId) {}

