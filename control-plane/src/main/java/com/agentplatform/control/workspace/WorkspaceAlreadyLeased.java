package com.agentplatform.control.workspace;

import java.util.UUID;

public final class WorkspaceAlreadyLeased extends RuntimeException {
  public WorkspaceAlreadyLeased(UUID workspaceId) {
    super("workspace has an active writer lease: " + workspaceId);
  }
}
