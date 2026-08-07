package com.agentplatform.control.run;

public enum SubagentAdmissionRejection {
  PARENT_NOT_RUNNING,
  DEPTH_LIMIT,
  ROLE_NOT_ALLOWED,
  PERMISSION_ESCALATION,
  CHILD_CAPACITY,
  BUDGET_EXHAUSTED,
  DELEGATION_CONFLICT
}
