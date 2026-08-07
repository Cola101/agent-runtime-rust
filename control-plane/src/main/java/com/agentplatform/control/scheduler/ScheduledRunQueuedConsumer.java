package com.agentplatform.control.scheduler;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.scheduling.annotation.Scheduled;

public final class ScheduledRunQueuedConsumer {
  private static final Logger LOGGER = LoggerFactory.getLogger(ScheduledRunQueuedConsumer.class);
  private final NatsRunQueuedConsumer consumer;
  private final SchedulerProperties properties;

  public ScheduledRunQueuedConsumer(
      NatsRunQueuedConsumer consumer, SchedulerProperties properties) {
    this.consumer = consumer;
    this.properties = properties;
  }

  @Scheduled(fixedDelayString = "${agent.runtime.scheduler.poll-delay-ms:100}")
  public SchedulerPollResult poll() {
    var result = consumer.poll(properties.getPollTimeout());
    if (result == SchedulerPollResult.TERMINATED) {
      LOGGER.warn("Terminated an invalid RunQueued message");
    }
    return result;
  }
}
