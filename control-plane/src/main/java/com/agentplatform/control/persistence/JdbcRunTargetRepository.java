package com.agentplatform.control.persistence;

import com.agentplatform.control.run.RunTarget;
import com.agentplatform.control.run.RunTargetRepository;
import java.util.List;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcRunTargetRepository implements RunTargetRepository {
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcRunTargetRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  @Override
  public List<RunTarget> findAvailable(UUID tenantId, UUID applicationId, int limit) {
    return transactions.execute(status -> {
      jdbc.queryForObject(
          "select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
      return jdbc.query("""
          select s.id as session_id,
                 w.id as workspace_id,
                 w.name as workspace_name,
                 av.id as agent_version_id,
                 a.name as agent_name,
                 av.version as agent_version,
                 mp.id as model_policy_id,
                 mp.name as model_policy_name
            from projects p
            join workspaces w
              on w.tenant_id = p.tenant_id and w.project_id = p.id
            join sessions s
              on s.tenant_id = w.tenant_id and s.workspace_id = w.id
            join agents a
              on a.tenant_id = w.tenant_id and a.workspace_id = w.id
            join agent_versions av
              on av.tenant_id = a.tenant_id and av.agent_id = a.id
            join model_policies mp
              on mp.tenant_id = w.tenant_id and mp.workspace_id = w.id
           where p.tenant_id = ?
             and p.application_id = ?
             and w.state = 'ready'
             and s.state = 'active'
           order by w.name, a.name, av.version desc, mp.name, s.created_at desc
           limit ?
          """, (row, rowNumber) -> new RunTarget(
              row.getObject("session_id", UUID.class),
              row.getObject("workspace_id", UUID.class),
              row.getString("workspace_name"),
              row.getObject("agent_version_id", UUID.class),
              row.getString("agent_name"),
              row.getInt("agent_version"),
              row.getObject("model_policy_id", UUID.class),
              row.getString("model_policy_name")),
          tenantId, applicationId, limit);
    });
  }
}
