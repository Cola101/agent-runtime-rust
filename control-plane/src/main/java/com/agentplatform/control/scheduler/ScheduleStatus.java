package com.agentplatform.control.scheduler;

public enum ScheduleStatus {
  DISPATCHED,
  ALREADY_DISPATCHED,
  RETRY_NO_CAPACITY,
  RETRY_WORKSPACE_BUSY,
  IGNORED_NOT_QUEUED
}
