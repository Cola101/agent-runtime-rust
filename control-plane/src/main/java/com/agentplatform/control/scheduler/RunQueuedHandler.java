package com.agentplatform.control.scheduler;

@FunctionalInterface
public interface RunQueuedHandler {
  ScheduleResult handle(QueuedRun queuedRun);
}
