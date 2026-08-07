package com.agentplatform.control.resource;

import java.util.List;

public record SubagentRoleDefinition(
    String name,
    String instructions,
    List<String> delegatedScopes) {
  public SubagentRoleDefinition {
    delegatedScopes = List.copyOf(delegatedScopes);
  }
}
