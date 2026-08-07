package com.agentplatform.control.outbox;

import java.time.Instant;
import java.util.UUID;

public record OutboxMessage(
    UUID tenantId,
    UUID id,
    String aggregateType,
    UUID aggregateId,
    String eventType,
    String payload,
    Instant createdAt,
    int publishAttempts,
    UUID claimToken) {}
