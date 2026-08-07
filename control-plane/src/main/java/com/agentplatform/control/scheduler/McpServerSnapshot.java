package com.agentplatform.control.scheduler;

import java.util.UUID;

/**
 * A federated MCP server as it is carried to the Worker (ADR-0040).
 *
 * <p>The envelope stays sealed here and travels base64-encoded. The Worker uses
 * the name to qualify tools as {@code mcp:<name>/<tool>} and the endpoint to
 * route, and it never holds anything that could authenticate to the server: that
 * is opened at the egress hop, the same way a model Provider credential is.
 */
public record McpServerSnapshot(
    UUID serverId, String name, String endpoint, String credentialEnvelopeBase64) {

  public McpServerSnapshot {
    if (credentialEnvelopeBase64 == null) {
      credentialEnvelopeBase64 = "";
    }
  }
}
