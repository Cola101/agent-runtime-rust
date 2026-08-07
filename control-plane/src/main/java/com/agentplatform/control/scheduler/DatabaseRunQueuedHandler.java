package com.agentplatform.control.scheduler;

import java.time.Duration;

public final class DatabaseRunQueuedHandler implements RunQueuedHandler {
  private final JdbcSchedulerRepository repository;
  private final Duration leaseDuration;
  private final Duration heartbeatFreshness;

  public DatabaseRunQueuedHandler(
      JdbcSchedulerRepository repository,
      Duration leaseDuration,
      Duration heartbeatFreshness) {
    this.repository = repository;
    this.leaseDuration = leaseDuration;
    this.heartbeatFreshness = heartbeatFreshness;
  }

  @Override
  public ScheduleResult handle(QueuedRun queuedRun) {
    return repository.schedule(
        queuedRun.tenantId(), queuedRun.runId(), leaseDuration, heartbeatFreshness);
  }
}
