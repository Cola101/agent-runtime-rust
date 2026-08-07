package com.agentplatform.control.scheduler;

import java.util.UUID;

public record QueuedRun(UUID tenantId, UUID runId) {}
