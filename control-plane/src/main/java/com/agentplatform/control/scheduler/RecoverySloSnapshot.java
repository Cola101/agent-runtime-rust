package com.agentplatform.control.scheduler;

public record RecoverySloSnapshot(
    int openIncidents,
    int overdueIncidents,
    int waitingCapacity,
    int recoveryRequested,
    long oldestOpenAgeMillis) {

  public RecoverySloSnapshot {
    if (openIncidents < 0 || overdueIncidents < 0 || waitingCapacity < 0
        || recoveryRequested < 0 || oldestOpenAgeMillis < 0) {
      throw new IllegalArgumentException("recovery SLO values must not be negative");
    }
    if (overdueIncidents > openIncidents
        || waitingCapacity + recoveryRequested != openIncidents) {
      throw new IllegalArgumentException("recovery SLO values are inconsistent");
    }
  }
}
