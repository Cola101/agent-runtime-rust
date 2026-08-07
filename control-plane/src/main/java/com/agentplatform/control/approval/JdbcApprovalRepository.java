package com.agentplatform.control.approval;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.sql.Timestamp;
import java.time.Instant;
import java.util.List;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcApprovalRepository implements ApprovalRepository {
  private static final ObjectMapper JSON = new ObjectMapper();
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcApprovalRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  @Override
  public List<ApprovalSummary> findPending(
      UUID tenantId, UUID applicationId, int limit) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      return jdbc.query("""
          select a.id,a.run_id,a.version,a.status,a.binding_digest,a.created_at,
                 a.policy_digest,a.session_scope_digest,a.policy_snapshot::text,
                 a.session_grant_eligible,
                 w.name as workspace_name,ag.name as agent_name,
                 a.request #>> '{approval,execution,call,name}' as tool_name,
                 a.request #>> '{approval,execution,call,id}' as tool_call_id,
                 a.request #>> '{approval,execution,effect}' as effect,
                 a.request #>> '{approval,execution,sandbox}' as sandbox,
                 (a.request #> '{approval,execution,call,arguments}')::text as arguments
            from approvals a
            join runs r on r.tenant_id = a.tenant_id and r.id = a.run_id
            join workspaces w on w.tenant_id = r.tenant_id and w.id = r.workspace_id
            join agent_versions av
              on av.tenant_id = r.tenant_id and av.id = r.agent_version_id
            join agents ag on ag.tenant_id = av.tenant_id and ag.id = av.agent_id
           where a.tenant_id = ? and r.application_id = ?
             and a.status = 'pending' and r.status = 'waiting_approval'
           order by a.created_at asc,a.id asc
           limit ?
          """, (row, rowNumber) -> new ApprovalSummary(
              row.getObject("id", UUID.class),
              row.getObject("run_id", UUID.class),
              row.getInt("version"),
              ApprovalStatus.valueOf(row.getString("status").toUpperCase()),
              row.getString("workspace_name"),
              row.getString("agent_name"),
              row.getString("tool_name"),
              row.getString("tool_call_id"),
              row.getString("effect"),
              row.getString("sandbox"),
              row.getString("binding_digest"),
              parseArguments(row.getString("arguments")),
              row.getTimestamp("created_at").toInstant(),
              row.getString("policy_digest"), row.getString("session_scope_digest"),
              parseNullableJson(row.getString("policy_snapshot")),
              row.getBoolean("session_grant_eligible")),
          tenantId, applicationId, limit);
    });
  }

  @Override
  public Approval decide(
      UUID tenantId, UUID applicationId, DecideApprovalCommand command) {
    return transactions.execute(
        status -> decideInTransaction(tenantId, applicationId, command));
  }

  private Approval decideInTransaction(
      UUID tenantId, UUID applicationId, DecideApprovalCommand command) {
    setTenant(tenantId);
    var targets = jdbc.query("""
        select a.run_id,a.version,a.status,a.attempt_id,a.worker_id,a.worker_incarnation_id,
               a.binding_digest,a.policy_digest,a.session_scope_digest,
               a.policy_snapshot::text,a.session_grant_eligible,
               a.policy_snapshot->>'tool_name' as tool_name,
               a.policy_snapshot->>'effect' as effect,
               a.policy_snapshot->>'sandbox' as sandbox,
               a.created_at,r.status as run_status,r.current_attempt_id,
               r.application_id,r.session_id,r.workspace_id,r.agent_version_id,
               s.state as session_state
          from approvals a
          join runs r on r.tenant_id = a.tenant_id and r.id = a.run_id
          join sessions s on s.tenant_id = r.tenant_id and s.id = r.session_id
         where a.tenant_id = ? and r.application_id = ? and a.id = ?
         for update of a,r
        """, (row, rowNumber) -> new DecisionTarget(
            row.getObject("run_id", UUID.class),
            row.getInt("version"),
            row.getString("status"),
            row.getObject("attempt_id", UUID.class),
            row.getObject("worker_id", UUID.class),
            row.getObject("worker_incarnation_id", UUID.class),
            row.getString("binding_digest"),
            row.getString("policy_digest"),
            row.getString("session_scope_digest"),
            row.getString("policy_snapshot"),
            row.getBoolean("session_grant_eligible"),
            row.getString("tool_name"), row.getString("effect"), row.getString("sandbox"),
            row.getTimestamp("created_at").toInstant(),
            row.getString("run_status"),
            row.getObject("current_attempt_id", UUID.class),
            row.getObject("application_id", UUID.class),
            row.getObject("session_id", UUID.class),
            row.getObject("workspace_id", UUID.class),
            row.getObject("agent_version_id", UUID.class),
            row.getString("session_state")),
        tenantId, applicationId, command.approvalId());
    if (targets.isEmpty()) {
      throw new ApprovalNotFound(command.approvalId());
    }
    var target = targets.getFirst();
    if (!"pending".equals(target.status())
        || target.version() != command.expectedVersion()
        || !"waiting_approval".equals(target.runStatus())
        || target.attemptId() == null
        || !target.attemptId().equals(target.currentAttemptId())
        || target.workerId() == null
        || target.workerIncarnationId() == null
        || target.bindingDigest() == null) {
      throw new ApprovalConflict(command.approvalId());
    }

    if (command.decision() == ApprovalDecision.ALLOW_SESSION) {
      if (!target.sessionGrantEligible()
          || target.policySnapshot() == null || target.policyDigest() == null
          || target.sessionScopeDigest() == null
          || !("pure".equals(target.effect()) || "idempotent".equals(target.effect()))
          || !"active".equals(target.sessionState())) {
        throw new ApprovalDecisionNotAllowed(command.approvalId());
      }
      jdbc.update("""
          insert into session_tool_grants (
            tenant_id,id,source_run_id,application_id,session_id,workspace_id,agent_version_id,
            scope_digest,policy_digest,policy_snapshot,tool_name,effect,sandbox,
            source_approval_id,created_by,created_at)
          values (?,?,?,?,?,?,?,?,?,?::jsonb,?,?,?,?,?,?)
          on conflict (tenant_id,session_id,agent_version_id,scope_digest,policy_digest)
          do nothing
          """, tenantId, UUID.randomUUID(), target.runId(), target.applicationId(), target.sessionId(),
          target.workspaceId(), target.agentVersionId(), target.sessionScopeDigest(),
          target.policyDigest(), target.policySnapshot(), target.toolName(), target.effect(),
          target.sandbox(), command.approvalId(), command.decidedBy(),
          Timestamp.from(command.decidedAt()));
    }

    var newVersion = target.version() + 1;
    var updated = jdbc.update("""
        update approvals
           set version = ?, status = ?,
               decision = jsonb_build_object(
                 'decision',cast(? as text),'reason',cast(? as text)),
               decided_by = ?, decided_at = ?
         where tenant_id = ? and id = ? and version = ? and status = 'pending'
        """, newVersion, command.decision().status().name().toLowerCase(),
        command.decision().value(), command.reason(), command.decidedBy(),
        Timestamp.from(command.decidedAt()), tenantId, command.approvalId(),
        command.expectedVersion());
    if (updated != 1) {
      throw new ApprovalConflict(command.approvalId());
    }

    var messageId = UUID.randomUUID();
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        values (?,?,'run',?,'tool.approval.decided',jsonb_build_object(
          'schema_version',2,
          'message_id',?,
          'tenant_id',?,
          'run_id',?,
          'attempt_id',?,
          'worker_id',?,
          'worker_incarnation_id',?,
          'approval_id',?,
          'approval_version',?,
          'binding_digest',?,
          'decision',?,
          'issued_at',?,
          'expires_at',?))
        """, tenantId, messageId, target.runId(), messageId.toString(), tenantId.toString(),
        target.runId().toString(), target.attemptId().toString(), target.workerId().toString(),
        target.workerIncarnationId().toString(), command.approvalId().toString(), newVersion,
        target.bindingDigest(),
        command.decision() == ApprovalDecision.ALLOW_SESSION
            ? ApprovalDecision.ALLOW_ONCE.value() : command.decision().value(),
        command.decidedAt().toString(),
        command.decidedAt().plusSeconds(300).toString());
    return new Approval(
        command.approvalId(), tenantId, target.runId(), newVersion,
        command.decision().status(), target.createdAt());
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject(
        "select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }

  private JsonNode parseArguments(String arguments) {
    if (arguments == null) {
      throw new IllegalStateException("pending approval is missing reviewed tool arguments");
    }
    try {
      return JSON.readTree(arguments);
    } catch (JsonProcessingException invalid) {
      throw new IllegalStateException("pending approval contains invalid tool arguments", invalid);
    }
  }

  private JsonNode parseNullableJson(String json) {
    if (json == null) {
      return null;
    }
    try {
      return JSON.readTree(json);
    } catch (JsonProcessingException invalid) {
      throw new IllegalStateException("approval contains an invalid policy snapshot", invalid);
    }
  }

  private record DecisionTarget(
      UUID runId,
      int version,
      String status,
      UUID attemptId,
      UUID workerId,
      UUID workerIncarnationId,
      String bindingDigest,
      String policyDigest,
      String sessionScopeDigest,
      String policySnapshot,
      boolean sessionGrantEligible,
      String toolName,
      String effect,
      String sandbox,
      Instant createdAt,
      String runStatus,
      UUID currentAttemptId,
      UUID applicationId,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      String sessionState) {}
}
