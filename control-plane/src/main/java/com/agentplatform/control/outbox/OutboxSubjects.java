package com.agentplatform.control.outbox;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.util.UUID;

public final class OutboxSubjects {
  public static final String RUNTIME_CONTROL_WILDCARD = "runtime.control.>";
  public static final String RUNTIME_EXECUTION_WILDCARD = "runtime.execution.>";
  public static final String RUN_QUEUED_V1 = "runtime.control.run.queued.v1";
  private static final ObjectMapper JSON = new ObjectMapper();

  private OutboxSubjects() {}

  public static String forMessage(OutboxMessage message) {
    if ("run.queued".equals(message.eventType())) {
      return RUN_QUEUED_V1;
    }
    if ("run.execution.requested".equals(message.eventType())) {
      return targeted(message.payload(), null, "run");
    }
    if ("run.recovery.requested".equals(message.eventType())) {
      return targeted(message.payload(), "execution", "restore");
    }
    if ("run.cancellation.requested".equals(message.eventType())) {
      return targeted(message.payload(), null, "cancel");
    }
    if ("run.steering.requested".equals(message.eventType())) {
      return targeted(message.payload(), null, "steer", 1);
    }
    if ("tool.approval.decided".equals(message.eventType())) {
      return targeted(message.payload(), null, "approval");
    }
    if ("workload.identity.renewed".equals(message.eventType())) {
      return targeted(message.payload(), null, "identity", 1);
    }
    throw new IllegalArgumentException("unsupported outbox event type " + message.eventType());
  }

  private static String targeted(String payload, String parent, String command) {
    return targeted(payload, parent, command, 2);
  }

  private static String targeted(String payload, String parent, String command, int version) {
    try {
      var root = JSON.readTree(payload);
      var target = parent == null ? root : root.path(parent);
      var workerId = UUID.fromString(target.path("worker_id").asText());
      var incarnationId = UUID.fromString(target.path("worker_incarnation_id").asText());
      return "runtime.execution.worker." + workerId + ".incarnation." + incarnationId
          + "." + command + ".v" + version;
    } catch (JsonProcessingException | IllegalArgumentException exception) {
      throw new IllegalArgumentException(
          "execution request has an invalid worker or incarnation id", exception);
    }
  }
}
