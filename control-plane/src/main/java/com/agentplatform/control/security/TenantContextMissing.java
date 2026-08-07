package com.agentplatform.control.security;

public final class TenantContextMissing extends RuntimeException {
  public TenantContextMissing() {
    super("access token must contain valid tenant_id and application_id claims");
  }
}
