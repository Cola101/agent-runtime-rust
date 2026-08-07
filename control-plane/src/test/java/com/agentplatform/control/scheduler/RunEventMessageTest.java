package com.agentplatform.control.scheduler;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatCode;

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

  /**
   * Every event the Rust kernel can emit has to be accepted here.
   *
   * <p>{@code tool.execution.auto_approved} was missing. The kernel emits it
   * whenever an approval policy exempts a call, so with auto-approval enabled
   * the control plane would have refused a legitimate event -- and refusing one
   * event mid-stream leaves a gap in the sequence that later reconciliation
   * reads as loss.
   */
  @Test
  void everyKernelToolLifecycleEventIsAccepted() {
    for (var type : new String[] {
        "tool.execution.requested", "tool.execution.started", "tool.execution.auto_approved",
        "tool.result", "tool.denied", "tool.retry_requested",
        "approval.required", "approval.rebound"}) {
      assertThatCode(() -> event(type))
          .as("event type %s", type)
          .doesNotThrowAnyException();
      assertThat(event(type).isTerminal()).as("event type %s", type).isFalse();
    }
  }

  private RunEventMessage event(String type) {
    return new RunEventMessage(
        UUID.randomUUID(), 1, UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), 1,
        UUID.randomUUID(), Instant.now(), UUID.randomUUID(), type, "{}", "0".repeat(64));
  }
}
