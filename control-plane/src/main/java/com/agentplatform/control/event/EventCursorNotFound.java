package com.agentplatform.control.event;

import java.util.UUID;

public final class EventCursorNotFound extends RuntimeException {
  public EventCursorNotFound(UUID eventId) {
    super("event cursor is not visible: " + eventId);
  }
}
