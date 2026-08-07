package com.agentplatform.control.outbox;

import java.time.Duration;
import java.util.Objects;
import java.util.UUID;

public final class OutboxPublisher {
  private final OutboxStore store;
  private final MessageBus messageBus;
  private final int batchSize;
  private final Duration claimDuration;

  public OutboxPublisher(
      OutboxStore store, MessageBus messageBus, int batchSize, Duration claimDuration) {
    this.store = Objects.requireNonNull(store);
    this.messageBus = Objects.requireNonNull(messageBus);
    if (batchSize < 1 || batchSize > 1000) {
      throw new IllegalArgumentException("outbox batch size must be between 1 and 1000");
    }
    this.batchSize = batchSize;
    this.claimDuration = Objects.requireNonNull(claimDuration);
    if (claimDuration.isZero() || claimDuration.isNegative()) {
      throw new IllegalArgumentException("outbox claim duration must be positive");
    }
  }

  public OutboxPublishResult publishNextBatch() {
    var claimToken = UUID.randomUUID();
    var messages = store.claimNext(batchSize, claimToken, claimDuration);
    var published = 0;
    var failed = 0;
    for (var message : messages) {
      try {
        messageBus.publish(message);
        if (!store.markPublished(message.tenantId(), message.id(), message.claimToken())) {
          throw new IllegalStateException("outbox claim was lost after publishing " + message.id());
        }
        published++;
      } catch (RuntimeException exception) {
        failed++;
        store.release(
            message.tenantId(), message.id(), message.claimToken(), failureMessage(exception));
      }
    }
    return new OutboxPublishResult(messages.size(), published, failed);
  }

  private String failureMessage(RuntimeException exception) {
    var message = exception.getMessage();
    return message == null || message.isBlank() ? exception.getClass().getSimpleName() : message;
  }
}
