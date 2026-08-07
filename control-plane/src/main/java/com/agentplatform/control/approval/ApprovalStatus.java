package com.agentplatform.control.approval;

import com.fasterxml.jackson.annotation.JsonValue;

public enum ApprovalStatus {
  PENDING,
  APPROVED,
  DENIED,
  EXPIRED;

  @JsonValue
  public String value() {
    return name().toLowerCase();
  }
}
