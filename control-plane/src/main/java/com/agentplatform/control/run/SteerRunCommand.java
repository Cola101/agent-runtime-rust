package com.agentplatform.control.run;

import java.nio.charset.StandardCharsets;

public record SteerRunCommand(String input) {
  public SteerRunCommand {
    if (input == null || input.isBlank()) {
      throw new InvalidRunSteering("steering input is required");
    }
    if (input.getBytes(StandardCharsets.UTF_8).length > 32 * 1024) {
      throw new InvalidRunSteering("steering input must not exceed 32768 UTF-8 bytes");
    }
  }
}
