package com.agentplatform.control.run;

import java.time.Duration;
import java.util.UUID;

public final class RunSteeringRateLimited extends RuntimeException {
  private final long retryAfterSeconds;

  public RunSteeringRateLimited(UUID runId, Duration retryAfter) {
    super("run %s accepts at most one new steering command every 2 seconds".formatted(runId));
    var retryAfterMillis = Math.max(1L, retryAfter.toMillis());
    this.retryAfterSeconds = Math.max(1L, (retryAfterMillis + 999L) / 1_000L);
  }

  public long retryAfterSeconds() {
    return retryAfterSeconds;
  }
}
