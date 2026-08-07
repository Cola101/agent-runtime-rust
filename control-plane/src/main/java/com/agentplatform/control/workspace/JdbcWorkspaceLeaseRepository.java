package com.agentplatform.control.workspace;

import java.time.Duration;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcWorkspaceLeaseRepository {
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcWorkspaceLeaseRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  public WorkspaceLease acquire(
      UUID tenantId, UUID workspaceId, UUID ownerId, Duration duration) {
    validateDuration(duration);
    return transactions.execute(status -> {
      setTenant(tenantId);
      var fencingToken = UUID.randomUUID();
      var leases = jdbc.query("""
          insert into workspace_leases (
            tenant_id, workspace_id, owner_id, owner_epoch, fencing_token, expires_at)
          values (?, ?, ?, 1, ?, now() + (? * interval '1 millisecond'))
          on conflict (tenant_id, workspace_id) do update
             set owner_id = excluded.owner_id,
                 owner_epoch = workspace_leases.owner_epoch + 1,
                 fencing_token = excluded.fencing_token,
                 expires_at = excluded.expires_at,
                 updated_at = now()
           where workspace_leases.expires_at <= now()
          returning tenant_id,workspace_id,owner_id,owner_epoch,fencing_token,expires_at
          """, (row, rowNumber) -> new WorkspaceLease(
              row.getObject("tenant_id", UUID.class),
              row.getObject("workspace_id", UUID.class),
              row.getObject("owner_id", UUID.class),
              row.getLong("owner_epoch"),
              row.getObject("fencing_token", UUID.class),
              row.getTimestamp("expires_at").toInstant()),
          tenantId, workspaceId, ownerId, fencingToken, duration.toMillis());
      if (leases.isEmpty()) throw new WorkspaceAlreadyLeased(workspaceId);
      return leases.getFirst();
    });
  }

  public boolean renew(WorkspaceLease lease, Duration duration) {
    validateDuration(duration);
    return Boolean.TRUE.equals(transactions.execute(status -> {
      setTenant(lease.tenantId());
      return jdbc.update("""
          update workspace_leases
             set expires_at = now() + (? * interval '1 millisecond'), updated_at = now()
           where tenant_id = ? and workspace_id = ? and owner_id = ?
             and owner_epoch = ? and fencing_token = ? and expires_at > now()
          """, duration.toMillis(), lease.tenantId(), lease.workspaceId(), lease.ownerId(),
          lease.ownerEpoch(), lease.fencingToken()) == 1;
    }));
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject("select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }

  private void validateDuration(Duration duration) {
    if (duration == null || duration.isNegative() || duration.isZero()
        || duration.compareTo(Duration.ofMinutes(5)) > 0) {
      throw new IllegalArgumentException("lease duration must be between 1ms and 5 minutes");
    }
  }
}
