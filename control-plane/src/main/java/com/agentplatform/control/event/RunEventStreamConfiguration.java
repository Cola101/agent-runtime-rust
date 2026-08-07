package com.agentplatform.control.event;

import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

@Configuration
class RunEventStreamConfiguration {
  @Bean(destroyMethod = "shutdown")
  ScheduledExecutorService runEventPoller() {
    return Executors.newScheduledThreadPool(4, Thread.ofPlatform()
        .name("run-event-poller-", 0)
        .daemon(true)
        .factory());
  }
}
