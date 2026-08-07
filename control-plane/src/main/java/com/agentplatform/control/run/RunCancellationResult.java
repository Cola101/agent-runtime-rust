package com.agentplatform.control.run;

import java.util.UUID;

public record RunCancellationResult(UUID runId, RunStatus status) {}
