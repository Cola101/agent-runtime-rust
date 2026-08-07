package com.agentplatform.control.outbox;

public record OutboxPublishResult(int claimed, int published, int failed) {}
