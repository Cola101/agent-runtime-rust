package com.agentplatform.control.run;

/**
 * The tenant is already at an admission limit.
 *
 * <p>Distinct from an invalid request: nothing about the request is wrong, and
 * the same request will succeed once capacity frees. That is why it carries a
 * retry hint and maps to 429 rather than 400 -- a client told "bad request"
 * would fix something that is not broken.
 */
public final class TenantQuotaExceeded extends RuntimeException {
  private final long retryAfterSeconds;

  public TenantQuotaExceeded(String message, long retryAfterSeconds) {
    super(message);
    this.retryAfterSeconds = retryAfterSeconds;
  }

  public long retryAfterSeconds() {
    return retryAfterSeconds;
  }
}
