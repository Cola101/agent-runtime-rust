package com.agentplatform.control.approval;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

public enum ApprovalDecision {
  ALLOW_ONCE("allow_once", ApprovalStatus.APPROVED),
  ALLOW_SESSION("allow_session", ApprovalStatus.APPROVED),
  DENY("deny", ApprovalStatus.DENIED);

  private final String value;
  private final ApprovalStatus status;

  ApprovalDecision(String value, ApprovalStatus status) {
    this.value = value;
    this.status = status;
  }

  @JsonValue
  public String value() {
    return value;
  }

  public ApprovalStatus status() {
    return status;
  }

  @JsonCreator
  public static ApprovalDecision fromValue(String value) {
    for (var decision : values()) {
      if (decision.value.equals(value)) {
        return decision;
      }
    }
    throw new IllegalArgumentException("unsupported approval decision " + value);
  }
}
