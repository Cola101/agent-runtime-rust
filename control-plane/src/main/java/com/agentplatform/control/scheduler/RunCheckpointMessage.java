package com.agentplatform.control.scheduler;

import java.time.Instant;
import java.util.UUID;

public record RunCheckpointMessage(
    int schemaVersion,
    UUID checkpointId,
    UUID tenantId,
    UUID runId,
    UUID sessionId,
    UUID attemptId,
    long ownerEpoch,
    UUID fencingToken,
    long sequence,
    String status,
    String kernelDigest,
    String toolCatalogDigest,
    byte[] payload,
    String payloadRef,
    String payloadEncoding,
    String payloadDigest,
    String storedPayloadDigest,
    long uncompressedSize,
    long storedSize,
    Instant createdAt) {
  private static final int INLINE_CHECKPOINT_MAX_BYTES = 512 * 1024;
  private static final int CHECKPOINT_MAX_UNCOMPRESSED_BYTES = 16 * 1024 * 1024;

  public RunCheckpointMessage(
      int schemaVersion,
      UUID checkpointId,
      UUID tenantId,
      UUID runId,
      UUID sessionId,
      UUID attemptId,
      long ownerEpoch,
      UUID fencingToken,
      long sequence,
      String status,
      String kernelDigest,
      String toolCatalogDigest,
      byte[] payload,
      String payloadDigest,
      Instant createdAt) {
    this(schemaVersion, checkpointId, tenantId, runId, sessionId, attemptId, ownerEpoch,
        fencingToken, sequence, status, kernelDigest, toolCatalogDigest, payload, null, "identity",
        payloadDigest, payloadDigest, payload == null ? 0 : payload.length,
        payload == null ? 0 : payload.length, createdAt);
  }

  public RunCheckpointMessage {
    if ((schemaVersion != 1 && schemaVersion != 2)
        || checkpointId == null || tenantId == null || runId == null
        || sessionId == null || attemptId == null || ownerEpoch < 1 || fencingToken == null
        || sequence < 0 || status == null || kernelDigest == null || toolCatalogDigest == null
        || payloadDigest == null || storedPayloadDigest == null || payloadEncoding == null
        || uncompressedSize < 1 || uncompressedSize > CHECKPOINT_MAX_UNCOMPRESSED_BYTES
        || storedSize < 1 || createdAt == null) {
      throw new IllegalArgumentException("checkpoint message is invalid");
    }
    if (schemaVersion == 1) {
      if (payload == null || payloadRef != null || !"identity".equals(payloadEncoding)
          || payload.length > INLINE_CHECKPOINT_MAX_BYTES || payload.length != storedSize
          || payload.length != uncompressedSize || !payloadDigest.equals(storedPayloadDigest)) {
        throw new IllegalArgumentException("checkpoint message is invalid");
      }
    } else {
      var inline = payload != null;
      var external = payloadRef != null;
      if (inline == external || !"zstd".equals(payloadEncoding)
          || (inline && (payload.length > INLINE_CHECKPOINT_MAX_BYTES
              || payload.length != storedSize))
          || (external && !payloadRef.equals("checkpoint://sha256/" + storedPayloadDigest))) {
        throw new IllegalArgumentException("checkpoint message is invalid");
      }
    }
    payload = payload == null ? null : payload.clone();
  }

  @Override
  public byte[] payload() {
    return payload == null ? null : payload.clone();
  }
}
