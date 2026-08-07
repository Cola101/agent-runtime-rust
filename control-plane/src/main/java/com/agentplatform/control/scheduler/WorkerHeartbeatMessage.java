package com.agentplatform.control.scheduler;

import java.time.Instant;
import java.util.List;
import java.util.UUID;

public record WorkerHeartbeatMessage(
    int schemaVersion,
    UUID messageId,
    UUID workerId,
    UUID incarnationId,
    Instant occurredAt,
    List<String> placements,
    int capacity,
    int activeRuns,
    List<ActiveAssignmentMessage> activeAssignments,
    String runtimeVersion,
    boolean acceptingWork,
    Instant drainingSince,
    Instant drainDeadline) {

  public WorkerHeartbeatMessage {
    placements = List.copyOf(placements);
    activeAssignments = List.copyOf(activeAssignments);
    if (schemaVersion != 1 && schemaVersion != 2) {
      throw new IllegalArgumentException("unsupported worker heartbeat schema version " + schemaVersion);
    }
    if (schemaVersion == 2 && (incarnationId == null || incarnationId.equals(new UUID(0, 0)))) {
      throw new IllegalArgumentException("v2 heartbeat must identify one worker incarnation");
    }
    if (capacity < 1) {
      throw new IllegalArgumentException("worker capacity must be positive");
    }
    if (activeRuns < 0 || activeRuns > capacity) {
      throw new IllegalArgumentException("worker active runs must be between zero and capacity");
    }
    if (activeAssignments.size() > activeRuns) {
      throw new IllegalArgumentException("assignment details must not exceed active run count");
    }
    if (placements.isEmpty() || placements.stream().anyMatch(value ->
        !"cloud".equals(value) && !"edge".equals(value))) {
      throw new IllegalArgumentException("worker placements must contain cloud or edge");
    }
    if (runtimeVersion == null || runtimeVersion.isBlank()) {
      throw new IllegalArgumentException("worker runtime version must not be blank");
    }
    if (acceptingWork && (drainingSince != null || drainDeadline != null)) {
      throw new IllegalArgumentException("admitting worker must not carry drain metadata");
    }
    if (!acceptingWork && (drainingSince == null || drainDeadline == null
        || drainingSince.isAfter(occurredAt) || !drainDeadline.isAfter(drainingSince))) {
      throw new IllegalArgumentException("draining worker must carry one valid drain window");
    }
  }

  public WorkerHeartbeatMessage(
      int schemaVersion,
      UUID messageId,
      UUID workerId,
      UUID incarnationId,
      Instant occurredAt,
      List<String> placements,
      int capacity,
      int activeRuns,
      List<ActiveAssignmentMessage> activeAssignments,
      String runtimeVersion) {
    this(schemaVersion, messageId, workerId, incarnationId, occurredAt, placements, capacity,
        activeRuns, activeAssignments, runtimeVersion, true, null, null);
  }

  public WorkerHeartbeatMessage(
      int schemaVersion,
      UUID messageId,
      UUID workerId,
      Instant occurredAt,
      List<String> placements,
      int capacity,
      int activeRuns,
      List<ActiveAssignmentMessage> activeAssignments,
      String runtimeVersion) {
    this(schemaVersion, messageId, workerId, workerId, occurredAt, placements, capacity,
        activeRuns, activeAssignments, runtimeVersion, true, null, null);
  }

  public WorkerHeartbeatMessage(
      int schemaVersion,
      UUID messageId,
      UUID workerId,
      Instant occurredAt,
      List<String> placements,
      int capacity,
      int activeRuns,
      String runtimeVersion) {
    this(schemaVersion, messageId, workerId, workerId, occurredAt, placements, capacity, activeRuns,
        List.of(), runtimeVersion, true, null, null);
  }
}
