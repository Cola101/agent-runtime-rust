package com.agentplatform.control.persistence;

import com.agentplatform.control.run.SpawnSubagentCommand;
import com.agentplatform.control.run.SubagentAdmission;
import com.agentplatform.control.run.SubagentAdmissionRejected;
import com.agentplatform.control.run.SubagentAdmissionRejection;
import com.agentplatform.control.run.SubagentParentNotFound;
import java.sql.Timestamp;
import java.time.Instant;
import java.util.List;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcSubagentAdmissionRepository {
  private static final int MAX_ACTIVE_CHILDREN = 8;
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcSubagentAdmissionRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  public SubagentAdmission admit(
      UUID tenantId,
      UUID applicationId,
      UUID parentRunId,
      SpawnSubagentCommand command) {
    return transactions.execute(status -> admitInTransaction(
        tenantId, applicationId, parentRunId, command));
  }

  private SubagentAdmission admitInTransaction(
      UUID tenantId,
      UUID applicationId,
      UUID parentRunId,
      SpawnSubagentCommand command) {
    setTenant(tenantId);
    var parents = jdbc.query("""
        select r.id,r.session_id,r.workspace_id,r.agent_version_id,r.model_policy_id,
               r.status,r.priority,r.placement,r.max_tokens,r.max_cost_cents,
               r.max_duration_seconds,coalesce(r.root_run_id,r.id) root_run_id,
               r.subagent_depth,
               array(select jsonb_array_elements_text(
                 case when r.subagent_depth = 0
                   then coalesce(av.spec->'delegated_scopes','[]'::jsonb)
                   else coalesce(parent_role.role->'delegated_scopes','[]'::jsonb)
                 end) order by 1) effective_scopes
          from runs r
          join agent_versions av
            on av.tenant_id = r.tenant_id and av.id = r.agent_version_id
          left join lateral (
            select role from jsonb_array_elements(
              coalesce(av.spec->'subagent_roles','[]'::jsonb)) role
             where role->>'name' = r.agent_role limit 1
          ) parent_role on r.subagent_depth > 0
         where r.tenant_id = ? and r.application_id = ? and r.id = ?
           and (r.subagent_depth = 0 or parent_role.role is not null)
         for update of r
        """, (row, rowNumber) -> new ParentRun(
            row.getObject("id", UUID.class),
            row.getObject("session_id", UUID.class),
            row.getObject("workspace_id", UUID.class),
            row.getObject("agent_version_id", UUID.class),
            row.getObject("model_policy_id", UUID.class),
            row.getString("status"),
            row.getShort("priority"),
            row.getString("placement"),
            row.getLong("max_tokens"),
            row.getLong("max_cost_cents"),
            row.getLong("max_duration_seconds"),
            row.getObject("root_run_id", UUID.class),
            row.getInt("subagent_depth"),
            stringList(row.getArray("effective_scopes").getArray())),
        tenantId, applicationId, parentRunId);
    if (parents.isEmpty()) {
      throw new SubagentParentNotFound(parentRunId);
    }
    var parent = parents.getFirst();
    var existing = findByDelegation(tenantId, command.delegationId());
    if (!existing.isEmpty()) {
      return exactDuplicateOrReject(tenantId, parent, command, existing.getFirst());
    }
    if (!"running".equals(parent.status())) {
      throw rejected(SubagentAdmissionRejection.PARENT_NOT_RUNNING);
    }
    if (parent.depth() >= 3) {
      throw rejected(SubagentAdmissionRejection.DEPTH_LIMIT);
    }
    var roles = jdbc.query("""
        select role->>'name' name,
               array(select jsonb_array_elements_text(
                 coalesce(role->'delegated_scopes','[]'::jsonb)) order by 1) delegated_scopes
          from agent_versions av
          cross join lateral jsonb_array_elements(
            coalesce(av.spec->'subagent_roles','[]'::jsonb)) role
         where av.tenant_id = ? and av.id = ? and role->>'name' = ?
        """, (row, rowNumber) -> new Role(
            row.getString("name"),
            stringList(row.getArray("delegated_scopes").getArray())),
        tenantId, parent.agentVersionId(), command.role());
    if (roles.isEmpty()) {
      throw rejected(SubagentAdmissionRejection.ROLE_NOT_ALLOWED);
    }
    if (!parent.effectiveScopes().containsAll(roles.getFirst().delegatedScopes())) {
      throw rejected(SubagentAdmissionRejection.PERMISSION_ESCALATION);
    }
    var activeChildren = jdbc.queryForObject("""
        select count(*) from runs
         where tenant_id = ? and parent_run_id = ?
           and status in ('queued','running','waiting_approval','suspended')
        """, Integer.class, tenantId, parentRunId);
    if (activeChildren == null || activeChildren >= MAX_ACTIVE_CHILDREN) {
      throw rejected(SubagentAdmissionRejection.CHILD_CAPACITY);
    }
    var reserved = reservedBudget(tenantId, parentRunId);
    var remaining = remainingBudget(parent, reserved);
    if (command.maxTokens() > remaining.tokens()
        || command.maxCostCents() > remaining.costCents()
        || command.maxDurationSeconds() > remaining.durationSeconds()) {
      throw rejected(SubagentAdmissionRejection.BUDGET_EXHAUSTED);
    }

    var childRunId = UUID.randomUUID();
    var createdAt = Instant.now();
    var inserted = jdbc.update("""
        insert into runs (
          tenant_id,application_id,id,session_id,workspace_id,agent_version_id,
          model_policy_id,idempotency_key,input,status,priority,placement,
          max_tokens,max_cost_cents,max_duration_seconds,root_run_id,parent_run_id,
          delegation_id,subagent_depth,agent_role,created_at,updated_at)
        values (?,?,?,?,?,?,?,? ,?,'queued',?,?,?,?,?,?,?,?,?,?,?,?)
        on conflict do nothing
        """, tenantId, applicationId, childRunId, parent.sessionId(), parent.workspaceId(),
        parent.agentVersionId(), parent.modelPolicyId(), "subagent:" + command.delegationId(),
        command.input(), parent.priority(), parent.placement(), command.maxTokens(),
        command.maxCostCents(), command.maxDurationSeconds(), parent.rootRunId(), parent.id(),
        command.delegationId(), parent.depth() + 1, command.role(), Timestamp.from(createdAt),
        Timestamp.from(createdAt));
    if (inserted == 0) {
      var raced = findByDelegation(tenantId, command.delegationId());
      if (raced.isEmpty()) {
        throw new IllegalStateException("subagent insertion conflicted without a delegation outcome");
      }
      return exactDuplicateOrReject(tenantId, parent, command, raced.getFirst());
    }
    insertRunQueuedOutbox(
        tenantId, childRunId, parent, command, createdAt);
    return new SubagentAdmission(
        childRunId, parent.rootRunId(), parent.id(), command.delegationId(), parent.depth() + 1,
        command.role(), remaining.tokens() - command.maxTokens(),
        remaining.costCents() - command.maxCostCents(),
        remaining.durationSeconds() - command.maxDurationSeconds());
  }

  private List<ChildRun> findByDelegation(UUID tenantId, UUID delegationId) {
    return jdbc.query("""
        select id,root_run_id,parent_run_id,delegation_id,subagent_depth,agent_role,input,
               max_tokens,max_cost_cents,max_duration_seconds
          from runs where tenant_id = ? and delegation_id = ?
        """, (row, rowNumber) -> new ChildRun(
            row.getObject("id", UUID.class), row.getObject("root_run_id", UUID.class),
            row.getObject("parent_run_id", UUID.class),
            row.getObject("delegation_id", UUID.class), row.getInt("subagent_depth"),
            row.getString("agent_role"), row.getString("input"), row.getLong("max_tokens"),
            row.getLong("max_cost_cents"), row.getLong("max_duration_seconds")),
        tenantId, delegationId);
  }

  private SubagentAdmission exactDuplicateOrReject(
      UUID tenantId, ParentRun parent, SpawnSubagentCommand command, ChildRun child) {
    if (!child.parentRunId().equals(parent.id())
        || !child.role().equals(command.role())
        || !child.input().equals(command.input())
        || child.maxTokens() != command.maxTokens()
        || child.maxCostCents() != command.maxCostCents()
        || child.maxDurationSeconds() != command.maxDurationSeconds()) {
      throw rejected(SubagentAdmissionRejection.DELEGATION_CONFLICT);
    }
    var remaining = remainingBudget(parent, reservedBudget(tenantId, parent.id()));
    return new SubagentAdmission(
        child.id(), child.rootRunId(), child.parentRunId(), child.delegationId(), child.depth(),
        child.role(), remaining.tokens(), remaining.costCents(), remaining.durationSeconds());
  }

  private Budget reservedBudget(UUID tenantId, UUID parentRunId) {
    return jdbc.queryForObject("""
        select coalesce(sum(max_tokens),0),coalesce(sum(max_cost_cents),0),
               coalesce(sum(max_duration_seconds),0)
          from runs where tenant_id = ? and parent_run_id = ?
        """, (row, rowNumber) -> new Budget(
            row.getLong(1), row.getLong(2), row.getLong(3)), tenantId, parentRunId);
  }

  private Budget remainingBudget(ParentRun parent, Budget reserved) {
    if (reserved.tokens() > parent.maxTokens()
        || reserved.costCents() > parent.maxCostCents()
        || reserved.durationSeconds() > parent.maxDurationSeconds()) {
      throw rejected(SubagentAdmissionRejection.BUDGET_EXHAUSTED);
    }
    return new Budget(
        parent.maxTokens() - reserved.tokens(),
        parent.maxCostCents() - reserved.costCents(),
        parent.maxDurationSeconds() - reserved.durationSeconds());
  }

  private void insertRunQueuedOutbox(
      UUID tenantId,
      UUID childRunId,
      ParentRun parent,
      SpawnSubagentCommand command,
      Instant createdAt) {
    var outboxId = UUID.randomUUID();
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        values (?,?,'run',?,'run.queued',jsonb_build_object(
          'schema_version',1,'message_id',?::text,'tenant_id',?::text,
          'run_id',?::text,'session_id',?::text,'workspace_id',?::text,
          'agent_version_id',?::text,'model_policy_id',?::text,
          'occurred_at',?::text,'input',?,'priority','interactive',
          'placement',?,'budget',jsonb_build_object(
            'max_tokens',?,'max_cost_cents',?,'max_duration_seconds',?)))
        """, tenantId, outboxId, childRunId, outboxId.toString(), tenantId.toString(),
        childRunId.toString(), parent.sessionId().toString(), parent.workspaceId().toString(),
        parent.agentVersionId().toString(), parent.modelPolicyId().toString(),
        createdAt.toString(), command.input(), parent.placement(), command.maxTokens(),
        command.maxCostCents(), command.maxDurationSeconds());
  }

  private List<String> stringList(Object sqlArray) {
    if (sqlArray instanceof String[] values) return List.of(values);
    if (sqlArray instanceof Object[] values) {
      return java.util.Arrays.stream(values).map(Object::toString).toList();
    }
    throw new IllegalArgumentException("database scope array is invalid");
  }

  private SubagentAdmissionRejected rejected(SubagentAdmissionRejection reason) {
    return new SubagentAdmissionRejected(reason);
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject(
        "select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }

  private record ParentRun(
      UUID id,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID modelPolicyId,
      String status,
      short priority,
      String placement,
      long maxTokens,
      long maxCostCents,
      long maxDurationSeconds,
      UUID rootRunId,
      int depth,
      List<String> effectiveScopes) {}

  private record Role(String name, List<String> delegatedScopes) {}

  private record ChildRun(
      UUID id,
      UUID rootRunId,
      UUID parentRunId,
      UUID delegationId,
      int depth,
      String role,
      String input,
      long maxTokens,
      long maxCostCents,
      long maxDurationSeconds) {}

  private record Budget(long tokens, long costCents, long durationSeconds) {}
}
