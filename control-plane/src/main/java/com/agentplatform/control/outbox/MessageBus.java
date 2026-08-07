package com.agentplatform.control.outbox;

public interface MessageBus {
  void publish(OutboxMessage message);
}
