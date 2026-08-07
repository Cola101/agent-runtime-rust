package com.agentplatform.control.identity;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;
import java.util.Objects;

public final class WorkloadToken {
  private final String value;

  @JsonCreator
  public WorkloadToken(String value) {
    this.value = Objects.requireNonNull(value, "workload token");
    if (value.isBlank() || value.length() > 8192) {
      throw new IllegalArgumentException("workload token must contain 1-8192 characters");
    }
  }

  @JsonValue
  public String value() {
    return value;
  }

  @Override
  public String toString() {
    return "WorkloadToken[REDACTED]";
  }

  @Override
  public boolean equals(Object other) {
    return other instanceof WorkloadToken token && value.equals(token.value);
  }

  @Override
  public int hashCode() {
    return value.hashCode();
  }
}
