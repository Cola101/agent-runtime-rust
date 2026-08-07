package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.UUID;

public record ModelProviderResource(
    UUID id,
    String name,
    String protocol,
    String endpoint,
    String model,
    String state,
    String credentialStatus,
    Instant createdAt) {}
