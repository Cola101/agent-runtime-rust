package com.agentplatform.control.run;

import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

@Configuration
class RunConfiguration {
  @Bean
  RunService runService(RunRepository repository) {
    return new RunService(repository);
  }

  @Bean
  RunTargetService runTargetService(RunTargetRepository repository) {
    return new RunTargetService(repository);
  }
}
