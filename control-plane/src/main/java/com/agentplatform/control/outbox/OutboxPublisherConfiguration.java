package com.agentplatform.control.outbox;

import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.boot.context.properties.EnableConfigurationProperties;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.scheduling.TaskScheduler;
import org.springframework.scheduling.annotation.EnableScheduling;
import org.springframework.scheduling.concurrent.ThreadPoolTaskScheduler;

@Configuration(proxyBeanMethods = false)
@EnableScheduling
@EnableConfigurationProperties(OutboxProperties.class)
@ConditionalOnProperty(prefix = "agent.runtime.outbox", name = "enabled", havingValue = "true")
public class OutboxPublisherConfiguration {
  @Bean
  TaskScheduler outboxTaskScheduler() {
    var scheduler = new ThreadPoolTaskScheduler();
    scheduler.setPoolSize(1);
    scheduler.setThreadNamePrefix("outbox-publisher-");
    scheduler.setWaitForTasksToCompleteOnShutdown(true);
    return scheduler;
  }

  @Bean(destroyMethod = "close")
  MessageBus outboxMessageBus(OutboxProperties properties) {
    return NatsJetStreamMessageBus.connect(
        properties.getNatsSecurity().settingsFor(properties.getNatsUrl()));
  }

  @Bean
  OutboxPublisher outboxPublisher(
      OutboxStore store,
      MessageBus messageBus,
      OutboxProperties properties) {
    return new OutboxPublisher(
        store, messageBus, properties.getBatchSize(), properties.getClaimDuration());
  }

  @Bean
  ScheduledOutboxPublisher scheduledOutboxPublisher(OutboxPublisher publisher) {
    return new ScheduledOutboxPublisher(publisher);
  }
}
