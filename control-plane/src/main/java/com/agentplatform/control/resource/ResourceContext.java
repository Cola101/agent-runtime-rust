package com.agentplatform.control.resource;

import java.util.List;
import java.util.UUID;

public record ResourceContext(UUID applicationId, String applicationName, List<ProjectSummary> projects) {
  public ResourceContext {
    projects = List.copyOf(projects);
  }
}
