package com.agentplatform.control.outbox;

import java.sql.ResultSet;
import java.sql.SQLException;
import java.time.Duration;
import java.util.List;
import java.util.Objects;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcOutboxRepository implements OutboxStore {
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcOutboxRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = Objects.requireNonNull(jdbc);
    this.transactions = Objects.requireNonNull(transactions);
  }

  @Override
  public List<OutboxMessage> claimNext(int limit, UUID claimToken, Duration leaseDuration) {
    if (limit < 1 || limit > 1000) {
      throw new IllegalArgumentException("outbox batch size must be between 1 and 1000");
    }
    Objects.requireNonNull(claimToken, "claimToken");
    Objects.requireNonNull(leaseDuration, "leaseDuration");
    if (leaseDuration.isZero() || leaseDuration.isNegative()) {
      throw new IllegalArgumentException("outbox claim duration must be positive");
    }

    return transactions.execute(status -> jdbc.query("""
        with candidates as (
          select tenant_id, id
            from outbox_events
           where published_at is null
             and (claim_until is null or claim_until <= clock_timestamp())
           order by created_at, id
           for update skip locked
           limit ?
        )
        update outbox_events message
           set claim_token = ?,
               claim_until = clock_timestamp() + (? * interval '1 millisecond'),
               publish_attempts = publish_attempts + 1
          from candidates
         where message.tenant_id = candidates.tenant_id
           and message.id = candidates.id
        returning message.tenant_id, message.id, message.aggregate_type,
                  message.aggregate_id, message.event_type, message.payload::text,
                  message.created_at, message.publish_attempts, message.claim_token
        """, this::mapMessage, limit, claimToken, leaseDuration.toMillis()));
  }

  @Override
  public boolean markPublished(UUID tenantId, UUID messageId, UUID claimToken) {
    requireIdentity(tenantId, messageId, claimToken);
    return Boolean.TRUE.equals(transactions.execute(status -> jdbc.update("""
        update outbox_events
           set published_at = clock_timestamp(), claim_token = null, claim_until = null,
               last_error = null
         where tenant_id = ? and id = ? and claim_token = ? and published_at is null
        """, tenantId, messageId, claimToken) == 1));
  }

  @Override
  public boolean release(
      UUID tenantId, UUID messageId, UUID claimToken, String failureMessage) {
    requireIdentity(tenantId, messageId, claimToken);
    var boundedError = Objects.requireNonNullElse(failureMessage, "unknown publish failure");
    if (boundedError.length() > 2000) {
      boundedError = boundedError.substring(0, 2000);
    }
    var error = boundedError;
    return Boolean.TRUE.equals(transactions.execute(status -> jdbc.update("""
        update outbox_events
           set claim_token = null, claim_until = null, last_error = ?
         where tenant_id = ? and id = ? and claim_token = ? and published_at is null
        """, error, tenantId, messageId, claimToken) == 1));
  }

  private void requireIdentity(UUID tenantId, UUID messageId, UUID claimToken) {
    Objects.requireNonNull(tenantId, "tenantId");
    Objects.requireNonNull(messageId, "messageId");
    Objects.requireNonNull(claimToken, "claimToken");
  }

  private OutboxMessage mapMessage(ResultSet row, int rowNumber) throws SQLException {
    return new OutboxMessage(
        row.getObject("tenant_id", UUID.class),
        row.getObject("id", UUID.class),
        row.getString("aggregate_type"),
        row.getObject("aggregate_id", UUID.class),
        row.getString("event_type"),
        row.getString("payload"),
        row.getTimestamp("created_at").toInstant(),
        row.getInt("publish_attempts"),
        row.getObject("claim_token", UUID.class));
  }
}
