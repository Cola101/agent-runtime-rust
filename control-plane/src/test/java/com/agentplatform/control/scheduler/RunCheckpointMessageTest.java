package com.agentplatform.control.scheduler;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import java.time.Instant;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class RunCheckpointMessageTest {

  @Test
  void rejectsInlinePayloadThatWouldExceedTheDefaultNatsEnvelopeLimit() {
    assertThatThrownBy(() -> new RunCheckpointMessage(
        1, UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        UUID.randomUUID(), 1, UUID.randomUUID(), 1, "running", "a".repeat(64),
        "b".repeat(64), new byte[512 * 1024 + 1], "c".repeat(64), Instant.now()))
        .isInstanceOf(IllegalArgumentException.class);
  }

  @Test
  void acceptsV2ContentAddressedCheckpointReferenceWithoutEmbeddingTheObject() {
    var digest = "c".repeat(64);
    var checkpoint = new RunCheckpointMessage(
        2, UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        UUID.randomUUID(), 1, UUID.randomUUID(), 1, "running", "a".repeat(64),
        "b".repeat(64), null, "checkpoint://sha256/" + digest, "zstd",
        "d".repeat(64), digest, 900_000, 600_000, Instant.now());

    assertThat(checkpoint.payload()).isNull();
    assertThat(checkpoint.payloadRef()).isEqualTo("checkpoint://sha256/" + digest);
  }

  @Test
  void rejectsV2CheckpointThatEmbedsPayloadAndReferenceAtTheSameTime() {
    var digest = "c".repeat(64);
    assertThatThrownBy(() -> new RunCheckpointMessage(
        2, UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        UUID.randomUUID(), 1, UUID.randomUUID(), 1, "running", "a".repeat(64),
        "b".repeat(64), new byte[] {1}, "checkpoint://sha256/" + digest, "zstd",
        "d".repeat(64), digest, 900_000, 1, Instant.now()))
        .isInstanceOf(IllegalArgumentException.class);
  }
}
