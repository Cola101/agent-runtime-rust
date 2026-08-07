package com.agentplatform.control.event;

import com.agentplatform.control.run.RunNotFound;
import java.util.List;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcRunEventRepository {
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcRunEventRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  public List<RunEvent> findAfter(
      UUID tenantId, UUID applicationId, UUID runId, UUID lastEventId, int limit) {
    if (limit < 1 || limit > 1000) {
      throw new IllegalArgumentException("event replay limit must be between 1 and 1000");
    }
    return transactions.execute(status -> {
      setTenant(tenantId);
      ensureAuthorizedRun(tenantId, applicationId, runId);
      long afterSequence = lastEventId == null ? 0 : findSequence(tenantId, runId, lastEventId);
      return jdbc.query("""
          select event_id,tenant_id,run_id,session_id,sequence,attempt_id,occurred_at,
                 type,payload::text,digest
            from run_events
           where tenant_id = ? and run_id = ? and sequence > ?
           order by sequence
           limit ?
          """, (row, rowNumber) -> new RunEvent(
              row.getObject("event_id", UUID.class),
              row.getObject("tenant_id", UUID.class),
              row.getObject("run_id", UUID.class),
              row.getObject("session_id", UUID.class),
              row.getLong("sequence"),
              row.getObject("attempt_id", UUID.class),
              row.getTimestamp("occurred_at").toInstant(),
              row.getString("type"),
              row.getString("payload"),
              row.getString("digest")), tenantId, runId, afterSequence, limit);
    });
  }

  private void ensureAuthorizedRun(UUID tenantId, UUID applicationId, UUID runId) {
    var count = jdbc.queryForObject("""
        select count(*)
          from runs
         where tenant_id = ? and application_id = ? and id = ?
        """, Integer.class, tenantId, applicationId, runId);
    if (count == null || count == 0) {
      throw new RunNotFound(runId);
    }
  }

  private long findSequence(UUID tenantId, UUID runId, UUID eventId) {
    var sequences = jdbc.query(
        "select sequence from run_events where tenant_id = ? and run_id = ? and event_id = ?",
        (row, rowNumber) -> row.getLong(1), tenantId, runId, eventId);
    if (sequences.isEmpty()) {
      throw new EventCursorNotFound(eventId);
    }
    return sequences.getFirst();
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject("select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }
}
