package com.agentplatform.control.scheduler;

import java.time.Instant;
import java.util.Set;
import java.util.UUID;

public record RunEventMessage(
    UUID eventId,
    int schemaVersion,
    UUID tenantId,
    UUID sessionId,
    UUID runId,
    long sequence,
    UUID attemptId,
    Instant timestamp,
    UUID traceId,
    String type,
    String payload,
    String digest) {

  private static final Set<String> SUPPORTED_TYPES = Set.of(
      "run.started", "model.output.delta", "model.tool_call", "model.usage",
      "model.turn.completed", "tool.execution.requested", "tool.execution.started",
      "tool.result", "tool.denied",
      "tool.retry_requested", "approval.required", "approval.rebound",
      "subagent.spawn.requested", "subagent.result.received", "run.restored", "run.resumed",
      "run.steer.applied",
      "run.succeeded", "run.failed", "run.cancelled", "run.timed_out", "run.indeterminate");
  private static final Set<String> TERMINAL_TYPES = Set.of(
      "run.succeeded", "run.failed", "run.cancelled", "run.timed_out", "run.indeterminate");

  public RunEventMessage {
    if (eventId == null || tenantId == null || sessionId == null || runId == null
        || attemptId == null || timestamp == null || traceId == null) {
      throw new IllegalArgumentException("run event identity must be complete");
    }
    if (schemaVersion != 1 || sequence < 1) {
      throw new IllegalArgumentException("run event schema and sequence must be supported");
    }
    if (!SUPPORTED_TYPES.contains(type)) {
      throw new IllegalArgumentException("unsupported worker run event type " + type);
    }
    if (payload == null || payload.isBlank() || digest == null || digest.length() != 64) {
      throw new IllegalArgumentException("run event payload and digest must be complete");
    }
  }

  public boolean isTerminal() {
    return TERMINAL_TYPES.contains(type);
  }

  public String terminalStatus() {
    if (!isTerminal()) {
      throw new IllegalStateException("run event is not terminal");
    }
    return type.substring("run.".length());
  }
}
