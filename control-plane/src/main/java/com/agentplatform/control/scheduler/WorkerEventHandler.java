package com.agentplatform.control.scheduler;

public interface WorkerEventHandler {
  void onHeartbeat(WorkerHeartbeatMessage heartbeat);

  boolean onAccepted(ExecutionAcceptedMessage accepted);

  default boolean onRunEvent(RunEventMessage event) {
    return false;
  }

  default boolean onCheckpoint(RunCheckpointMessage checkpoint) {
    return false;
  }

  default void onSteeringOutcome(RunSteeringOutcomeMessage outcome) {}
}
