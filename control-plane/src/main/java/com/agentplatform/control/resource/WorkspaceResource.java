package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.UUID;

public record WorkspaceResource(
    UUID id, UUID projectId, String name, String state, Instant createdAt) {}
