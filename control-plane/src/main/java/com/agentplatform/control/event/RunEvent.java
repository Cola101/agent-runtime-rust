package com.agentplatform.control.event;

import java.time.Instant;
import java.util.UUID;

public record RunEvent(
    UUID eventId,
    UUID tenantId,
    UUID runId,
    UUID sessionId,
    long sequence,
    UUID attemptId,
    Instant occurredAt,
    String type,
    String payload,
    String digest) {}
