package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.UUID;

public record AgentResource(UUID id, UUID workspaceId, String name, Instant createdAt) {}
