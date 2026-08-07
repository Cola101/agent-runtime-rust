package com.agentplatform.control.scheduler;

public final class DatabaseWorkerEventHandler implements WorkerEventHandler {
  private final JdbcSchedulerRepository repository;
  private final java.time.Duration leaseDuration;

  public DatabaseWorkerEventHandler(
      JdbcSchedulerRepository repository, java.time.Duration leaseDuration) {
    this.repository = repository;
    this.leaseDuration = leaseDuration;
  }

  @Override
  public void onHeartbeat(WorkerHeartbeatMessage heartbeat) {
    repository.recordHeartbeat(heartbeat, leaseDuration);
  }

  @Override
  public boolean onAccepted(ExecutionAcceptedMessage accepted) {
    return repository.recordAcceptance(accepted);
  }

  @Override
  public boolean onRunEvent(RunEventMessage event) {
    return repository.recordRunEvent(event);
  }

  @Override
  public boolean onCheckpoint(RunCheckpointMessage checkpoint) {
    return repository.recordCheckpoint(checkpoint);
  }

  @Override
  public void onSteeringOutcome(RunSteeringOutcomeMessage outcome) {
    repository.recordSteeringOutcome(outcome);
  }
}
