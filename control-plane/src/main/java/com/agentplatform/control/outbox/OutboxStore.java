package com.agentplatform.control.outbox;

import java.time.Duration;
import java.util.List;
import java.util.UUID;

public interface OutboxStore {
  List<OutboxMessage> claimNext(int limit, UUID claimToken, Duration leaseDuration);

  boolean markPublished(UUID tenantId, UUID messageId, UUID claimToken);

  boolean release(UUID tenantId, UUID messageId, UUID claimToken, String failureMessage);
}
