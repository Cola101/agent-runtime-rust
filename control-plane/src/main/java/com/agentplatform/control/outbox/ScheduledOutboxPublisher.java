package com.agentplatform.control.outbox;

import java.util.Objects;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.scheduling.annotation.Scheduled;

public final class ScheduledOutboxPublisher {
  private static final Logger LOGGER = LoggerFactory.getLogger(ScheduledOutboxPublisher.class);
  private final OutboxPublisher publisher;

  public ScheduledOutboxPublisher(OutboxPublisher publisher) {
    this.publisher = Objects.requireNonNull(publisher);
  }

  @Scheduled(fixedDelayString = "${agent.runtime.outbox.poll-delay-ms:500}")
  public OutboxPublishResult poll() {
    var result = publisher.publishNextBatch();
    if (result.failed() > 0) {
      LOGGER.warn(
          "Outbox batch completed with failures: claimed={}, published={}, failed={}",
          result.claimed(), result.published(), result.failed());
    }
    return result;
  }
}
