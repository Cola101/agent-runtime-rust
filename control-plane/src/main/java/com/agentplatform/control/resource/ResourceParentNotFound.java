package com.agentplatform.control.resource;

public final class ResourceParentNotFound extends RuntimeException {
  public ResourceParentNotFound() {
    super("resource parent is absent or outside the authorized application");
  }
}
