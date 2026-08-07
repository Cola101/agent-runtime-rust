package com.agentplatform.control.outbox;

import static org.assertj.core.api.Assertions.assertThat;

import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class OutboxPublisherTest {
  @Test
  void successfulPublishMarksClaimComplete() {
    var message = message("run.queued");
    var store = new RecordingStore(List.of(message));
    var published = new ArrayList<UUID>();
    MessageBus bus = candidate -> published.add(candidate.id());
    var publisher = new OutboxPublisher(store, bus, 100, Duration.ofSeconds(30));

    var result = publisher.publishNextBatch();

    assertThat(result).isEqualTo(new OutboxPublishResult(1, 1, 0));
    assertThat(published).containsExactly(message.id());
    assertThat(store.completed).containsExactly(message.id());
    assertThat(store.released).isEmpty();
  }

  @Test
  void oneBrokerFailureReleasesOnlyThatMessageAndContinuesTheBatch() {
    var failed = message("run.queued");
    var succeeded = message("run.queued");
    var store = new RecordingStore(List.of(failed, succeeded));
    MessageBus bus = candidate -> {
      if (candidate.id().equals(failed.id())) {
        throw new MessagePublishException("NATS unavailable", new IllegalStateException("offline"));
      }
    };
    var publisher = new OutboxPublisher(store, bus, 100, Duration.ofSeconds(30));

    var result = publisher.publishNextBatch();

    assertThat(result).isEqualTo(new OutboxPublishResult(2, 1, 1));
    assertThat(store.completed).containsExactly(succeeded.id());
    assertThat(store.released).containsExactly(failed.id());
    assertThat(store.releaseErrors).containsExactly("NATS unavailable");
  }

  private OutboxMessage message(String eventType) {
    return new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(), eventType, "{}",
        Instant.parse("2026-07-31T08:30:00Z"), 1, UUID.randomUUID());
  }

  private static final class RecordingStore implements OutboxStore {
    private final List<OutboxMessage> claimed;
    private final List<UUID> completed = new ArrayList<>();
    private final List<UUID> released = new ArrayList<>();
    private final List<String> releaseErrors = new ArrayList<>();

    private RecordingStore(List<OutboxMessage> claimed) {
      this.claimed = claimed;
    }

    @Override
    public List<OutboxMessage> claimNext(int limit, UUID claimToken, Duration leaseDuration) {
      return claimed;
    }

    @Override
    public boolean markPublished(UUID tenantId, UUID messageId, UUID claimToken) {
      completed.add(messageId);
      return true;
    }

    @Override
    public boolean release(
        UUID tenantId, UUID messageId, UUID claimToken, String failureMessage) {
      released.add(messageId);
      releaseErrors.add(failureMessage);
      return true;
    }
  }
}
