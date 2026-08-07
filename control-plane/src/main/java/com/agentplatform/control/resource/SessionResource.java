package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.UUID;

public record SessionResource(
    UUID id, UUID workspaceId, String title, String state, Instant createdAt) {}
