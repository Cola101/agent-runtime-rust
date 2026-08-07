package com.agentplatform.control.run;

public enum RunStatus {
  QUEUED,
  RUNNING,
  WAITING_APPROVAL,
  SUSPENDED,
  SUCCEEDED,
  FAILED,
  CANCELLED,
  TIMED_OUT,
  INDETERMINATE
}
