package com.agentplatform.control.scheduler;

import static org.assertj.core.api.Assertions.assertThat;

import java.time.Instant;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class RunEventMessageTest {
  @Test
  void toolLoopLifecycleEventsAreAcceptedButOnlyRealRunTerminalsAreTerminal() {
    assertThat(event("model.turn.completed").isTerminal()).isFalse();
    assertThat(event("tool.execution.requested").isTerminal()).isFalse();
    assertThat(event("tool.result").isTerminal()).isFalse();
    assertThat(event("tool.denied").isTerminal()).isFalse();
    assertThat(event("run.succeeded").isTerminal()).isTrue();
    assertThat(event("run.indeterminate").isTerminal()).isTrue();
    assertThat(event("run.steer.applied").isTerminal()).isFalse();
  }

  private RunEventMessage event(String type) {
    return new RunEventMessage(
        UUID.randomUUID(), 1, UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), 1,
        UUID.randomUUID(), Instant.now(), UUID.randomUUID(), type, "{}", "0".repeat(64));
  }
}
