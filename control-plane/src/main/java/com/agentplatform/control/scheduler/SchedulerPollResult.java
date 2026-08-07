package com.agentplatform.control.scheduler;

public enum SchedulerPollResult {
  IDLE,
  ACKED,
  RETRY_SCHEDULED,
  TERMINATED
}
