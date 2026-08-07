package com.agentplatform.control.resource;

import java.time.Instant;
import java.util.UUID;

/**
 * A tenant-registered MCP server (ADR-0040).
 *
 * <p>{@code credentialStatus} is a word, never the credential. It says whether
 * one was sealed at registration, which is what a tenant needs to see, and it
 * cannot become a leak by being logged.
 */
public record McpServerResource(
    UUID id,
    String name,
    String endpoint,
    String state,
    String credentialStatus,
    Instant createdAt) {}
