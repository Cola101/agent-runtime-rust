package com.agentplatform.control.run;

public final class RunTargetNotFound extends RuntimeException {
  public RunTargetNotFound() {
    super("the requested run target is not available to the authorized application");
  }
}
