package com.agentplatform.control.resource;

public final class ResourceConflict extends RuntimeException {
  public ResourceConflict() {
    super("a resource with the same name already exists under this parent");
  }
}
