package com.agentplatform.control.scheduler;

import java.time.Clock;
import org.springframework.boot.autoconfigure.condition.ConditionalOnMissingBean;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.boot.context.properties.EnableConfigurationProperties;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.scheduling.TaskScheduler;
import org.springframework.scheduling.annotation.EnableScheduling;
import org.springframework.scheduling.concurrent.ThreadPoolTaskScheduler;

@Configuration(proxyBeanMethods = false)
@EnableScheduling
@EnableConfigurationProperties(SchedulerProperties.class)
@ConditionalOnProperty(prefix = "agent.runtime.scheduler", name = "enabled", havingValue = "true")
public class SchedulerConfiguration {
  @Bean(name = "taskScheduler")
  TaskScheduler schedulerTaskScheduler() {
    var scheduler = new ThreadPoolTaskScheduler();
    scheduler.setPoolSize(3);
    scheduler.setThreadNamePrefix("run-scheduler-");
    scheduler.setWaitForTasksToCompleteOnShutdown(true);
    return scheduler;
  }

  @Bean
  @ConditionalOnMissingBean(RunQueuedHandler.class)
  RunQueuedHandler databaseRunQueuedHandler(
      JdbcSchedulerRepository repository, SchedulerProperties properties) {
    return new DatabaseRunQueuedHandler(
        repository, properties.getLeaseDuration(), properties.getHeartbeatFreshness());
  }

  @Bean
  @ConditionalOnMissingBean(WorkerEventHandler.class)
  WorkerEventHandler databaseWorkerEventHandler(
      JdbcSchedulerRepository repository, SchedulerProperties properties) {
    return new DatabaseWorkerEventHandler(repository, properties.getLeaseDuration());
  }

  @Bean
  @ConditionalOnMissingBean(DispatchReconciler.class)
  DispatchReconciler databaseDispatchReconciler(
      JdbcSchedulerRepository repository, SchedulerProperties properties) {
    return new DatabaseDispatchReconciler(
        repository, properties.getLeaseDuration(), properties.getHeartbeatFreshness());
  }

  @Bean(destroyMethod = "close")
  NatsRunQueuedConsumer natsRunQueuedConsumer(
      RunQueuedHandler handler, SchedulerProperties properties) {
    return NatsRunQueuedConsumer.connect(
        properties.getNatsSecurity().settingsFor(properties.getNatsUrl()),
        properties.getDurableName(),
        handler,
        properties.getRetryDelay());
  }

  @Bean(destroyMethod = "close")
  NatsWorkerEventConsumer natsWorkerEventConsumer(
      WorkerEventHandler handler, SchedulerProperties properties) {
    return NatsWorkerEventConsumer.connect(
        properties.getNatsSecurity().settingsFor(properties.getNatsUrl()),
        properties.getDurableName() + "-worker-events",
        handler);
  }

  @Bean
  ScheduledRunQueuedConsumer scheduledRunQueuedConsumer(
      NatsRunQueuedConsumer consumer, SchedulerProperties properties) {
    return new ScheduledRunQueuedConsumer(consumer, properties);
  }

  @Bean
  ScheduledWorkerEventConsumer scheduledWorkerEventConsumer(
      NatsWorkerEventConsumer consumer, SchedulerProperties properties) {
    return new ScheduledWorkerEventConsumer(consumer, properties);
  }

  @Bean
  ScheduledDispatchReconciler scheduledDispatchReconciler(DispatchReconciler reconciler) {
    return new ScheduledDispatchReconciler(reconciler);
  }

  @Bean
  RecoveryMetricsCollector recoveryMetricsCollector(
      RecoveryMetricsSource source, SchedulerProperties properties) {
    return new RecoveryMetricsCollector(
        source, properties.getRecoveryObjective(), Clock.systemUTC());
  }
}
