package com.agentplatform.control.resource;

import java.util.UUID;

@FunctionalInterface
public interface ProviderCredentialSealer {
  String seal(UUID tenantId, UUID providerId, String credential);
}
