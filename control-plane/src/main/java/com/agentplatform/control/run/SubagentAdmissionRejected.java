package com.agentplatform.control.run;

public final class SubagentAdmissionRejected extends RuntimeException {
  private final SubagentAdmissionRejection reason;

  public SubagentAdmissionRejected(SubagentAdmissionRejection reason) {
    super("subagent admission rejected: " + reason.name().toLowerCase());
    this.reason = reason;
  }

  public SubagentAdmissionRejection reason() {
    return reason;
  }
}
