package com.agentplatform.control.security;

import java.util.UUID;
import org.springframework.security.oauth2.jwt.Jwt;

public record TenantContext(UUID tenantId, UUID applicationId, String subject) {
  public static TenantContext from(Jwt jwt) {
    if (jwt == null) throw new TenantContextMissing();
    return new TenantContext(
        requiredUuid(jwt, "tenant_id"),
        requiredUuid(jwt, "application_id"),
        jwt.getSubject());
  }

  private static UUID requiredUuid(Jwt jwt, String claim) {
    var value = jwt.getClaimAsString(claim);
    if (value == null || value.isBlank()) throw new TenantContextMissing();
    try {
      return UUID.fromString(value);
    } catch (IllegalArgumentException invalid) {
      throw new TenantContextMissing();
    }
  }
}
