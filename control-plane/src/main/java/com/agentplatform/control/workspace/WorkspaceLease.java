package com.agentplatform.control.workspace;

import java.time.Instant;
import java.util.UUID;

public record WorkspaceLease(
    UUID tenantId,
    UUID workspaceId,
    UUID ownerId,
    long ownerEpoch,
    UUID fencingToken,
    Instant expiresAt) {}
