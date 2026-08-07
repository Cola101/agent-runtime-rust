package com.agentplatform.control.outbox;

public final class MessagePublishException extends RuntimeException {
  public MessagePublishException(String message, Throwable cause) {
    super(message, cause);
  }
}
