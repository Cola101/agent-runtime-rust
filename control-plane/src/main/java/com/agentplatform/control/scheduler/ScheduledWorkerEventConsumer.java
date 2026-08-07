package com.agentplatform.control.scheduler;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.scheduling.annotation.Scheduled;

public final class ScheduledWorkerEventConsumer {
  private static final Logger LOGGER = LoggerFactory.getLogger(ScheduledWorkerEventConsumer.class);
  private final NatsWorkerEventConsumer consumer;
  private final SchedulerProperties properties;

  public ScheduledWorkerEventConsumer(
      NatsWorkerEventConsumer consumer, SchedulerProperties properties) {
    this.consumer = consumer;
    this.properties = properties;
  }

  @Scheduled(fixedDelayString = "${agent.runtime.scheduler.worker-event-poll-delay-ms:100}")
  public WorkerEventPollResult poll() {
    var result = consumer.poll(properties.getPollTimeout());
    if (result == WorkerEventPollResult.TERMINATED) {
      LOGGER.warn("Terminated an invalid or unmatched worker event");
    }
    return result;
  }
}
