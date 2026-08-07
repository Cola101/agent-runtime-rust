package com.agentplatform.control.scheduler;

import java.util.Optional;

public record ScheduleResult(ScheduleStatus status, Optional<RunExecutionCommand> command) {
  public static ScheduleResult withoutCommand(ScheduleStatus status) {
    return new ScheduleResult(status, Optional.empty());
  }

  public static ScheduleResult withCommand(
      ScheduleStatus status, RunExecutionCommand command) {
    return new ScheduleResult(status, Optional.of(command));
  }
}
