package com.agentplatform.control.persistence;

import com.agentplatform.control.run.Run;
import com.agentplatform.control.run.TenantQuotaExceeded;
import com.agentplatform.control.run.RunRepository;
import com.agentplatform.control.run.RunAlreadyTerminal;
import com.agentplatform.control.run.RunNotFound;
import com.agentplatform.control.run.RunStatus;
import com.agentplatform.control.run.RunSummary;
import com.agentplatform.control.run.RunSteeringConflict;
import com.agentplatform.control.run.RunSteeringNotAllowed;
import com.agentplatform.control.run.RunSteeringRateLimited;
import com.agentplatform.control.run.RunSteeringResult;
import com.agentplatform.control.run.RunTargetNotFound;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Timestamp;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.time.Duration;
import java.time.Instant;
import java.util.HexFormat;
import java.util.Optional;
import java.util.List;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcRunRepository implements RunRepository {
  /** Applied to a tenant with no configured limit. */
  private static final int DEFAULT_MAX_ACTIVE_RUNS = 64;
  /** Long enough that a client retrying is not itself the load problem. */
  private static final long TENANT_QUOTA_RETRY_AFTER_SECONDS = 30;

  private static final String SELECT_BY_KEY = """
      select id, tenant_id, session_id, agent_version_id, workspace_id, model_policy_id, idempotency_key,
             input, status, max_tokens, max_cost_cents, max_duration_seconds, created_at
        from runs
       where tenant_id = ? and application_id = ? and idempotency_key = ?
      """;

  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcRunRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  @Override
  public Optional<Run> findByIdempotencyKey(
      UUID tenantId, UUID applicationId, String idempotencyKey) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      return find(tenantId, applicationId, idempotencyKey);
    });
  }

  @Override
  public Run save(UUID applicationId, Run run) {
    return transactions.execute(status -> {
      setTenant(run.tenantId());
      ensureAuthorizedTarget(applicationId, run);
      admitWithinTenantQuota(run.tenantId(), applicationId, run.idempotencyKey());
      int inserted = jdbc.update("""
          insert into runs (
            tenant_id, application_id, id, session_id, workspace_id, agent_version_id,
            model_policy_id, idempotency_key,
            input, status, max_tokens, max_cost_cents, max_duration_seconds, created_at, updated_at)
          values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
          on conflict (tenant_id, application_id, idempotency_key) do nothing
          """,
          run.tenantId(), applicationId, run.id(), run.sessionId(), run.workspaceId(), run.agentVersionId(),
          run.modelPolicyId(), run.idempotencyKey(), run.input(), run.status().name().toLowerCase(), run.maxTokens(),
          run.maxCostCents(), run.maxDurationSeconds(), Timestamp.from(run.createdAt()),
          Timestamp.from(run.createdAt()));
      if (inserted == 1) {
        var outboxId = UUID.randomUUID();
        jdbc.update("""
            insert into outbox_events (
              tenant_id, id, aggregate_type, aggregate_id, event_type, payload)
            values (?, ?, 'run', ?, 'run.queued', jsonb_build_object(
              'schema_version', 1,
              'message_id', ?,
              'tenant_id', ?,
              'run_id', ?,
              'session_id', ?,
              'workspace_id', ?,
              'agent_version_id', ?,
              'model_policy_id', ?,
              'occurred_at', ?,
              'input', ?,
              'priority', 'interactive',
              'placement', 'cloud',
              'budget', jsonb_build_object(
                'max_tokens', ?,
                'max_cost_cents', ?,
                'max_duration_seconds', ?
              )
            ))
            """,
            run.tenantId(), outboxId, run.id(), outboxId.toString(), run.tenantId().toString(),
            run.id().toString(), run.sessionId().toString(), run.workspaceId().toString(),
            run.agentVersionId().toString(), run.modelPolicyId().toString(),
            run.createdAt().toString(), run.input(), run.maxTokens(),
            run.maxCostCents(), run.maxDurationSeconds());
        return run;
      }
      return find(run.tenantId(), applicationId, run.idempotencyKey())
          .orElseThrow(() -> new IllegalStateException("conflicting run disappeared"));
    });
  }

  private void ensureAuthorizedTarget(UUID applicationId, Run run) {
    var authorized = jdbc.queryForObject("""
        select exists (
          select 1
            from sessions s
            join workspaces w
              on w.tenant_id = s.tenant_id and w.id = s.workspace_id
            join projects p
              on p.tenant_id = w.tenant_id and p.id = w.project_id
            join agents a
              on a.tenant_id = w.tenant_id and a.workspace_id = w.id
            join agent_versions av
              on av.tenant_id = a.tenant_id and av.agent_id = a.id
            join model_policies mp
              on mp.tenant_id = w.tenant_id and mp.workspace_id = w.id
           where s.tenant_id = ? and p.application_id = ?
             and s.id = ? and s.workspace_id = ? and s.state = 'active'
             and w.id = ? and w.state = 'ready'
             and av.id = ? and mp.id = ?
        )
        """, Boolean.class, run.tenantId(), applicationId, run.sessionId(), run.workspaceId(),
        run.workspaceId(), run.agentVersionId(), run.modelPolicyId());
    if (!Boolean.TRUE.equals(authorized)) {
      throw new RunTargetNotFound();
    }
  }

  @Override
  public RunStatus requestCancellation(UUID tenantId, UUID runId, Instant requestedAt) {
    return requestCancellation(tenantId, null, runId, requestedAt);
  }

  @Override
  public RunStatus requestCancellation(
      UUID tenantId, UUID applicationId, UUID runId, Instant requestedAt) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      var targets = applicationId == null ? jdbc.query("""
          select r.status,r.session_id,r.current_attempt_id,d.worker_id,d.worker_incarnation_id
            from runs r
            left join run_dispatches d
              on d.tenant_id = r.tenant_id and d.run_id = r.id
             and d.attempt_id = r.current_attempt_id
           where r.tenant_id = ? and r.id = ?
           for update of r
          """, (row, rowNumber) -> new CancellationTarget(
              RunStatus.valueOf(row.getString("status").toUpperCase()),
              row.getObject("session_id", UUID.class),
              row.getObject("current_attempt_id", UUID.class),
              row.getObject("worker_id", UUID.class),
              row.getObject("worker_incarnation_id", UUID.class)), tenantId, runId)
          : jdbc.query("""
              select r.status,r.session_id,r.current_attempt_id,d.worker_id,d.worker_incarnation_id
                from runs r
                left join run_dispatches d
                  on d.tenant_id = r.tenant_id and d.run_id = r.id
                 and d.attempt_id = r.current_attempt_id
               where r.tenant_id = ? and r.application_id = ? and r.id = ?
               for update of r
              """, (row, rowNumber) -> new CancellationTarget(
                  RunStatus.valueOf(row.getString("status").toUpperCase()),
                  row.getObject("session_id", UUID.class),
                  row.getObject("current_attempt_id", UUID.class),
                  row.getObject("worker_id", UUID.class),
                  row.getObject("worker_incarnation_id", UUID.class)),
              tenantId, applicationId, runId);
      if (targets.isEmpty()) {
        throw new RunNotFound(runId);
      }
      var target = targets.getFirst();
      if (isTerminal(target.status())) {
        throw new RunAlreadyTerminal(runId, target.status());
      }
      if (target.status() == RunStatus.QUEUED && target.attemptId() == null) {
        cancelUndispatchedRun(tenantId, runId, target.sessionId(), requestedAt);
        return RunStatus.CANCELLED;
      }
      if ((target.status() != RunStatus.QUEUED
          && target.status() != RunStatus.RUNNING
          && target.status() != RunStatus.WAITING_APPROVAL
          && target.status() != RunStatus.SUSPENDED)
          || target.attemptId() == null || target.workerId() == null
          || target.workerIncarnationId() == null) {
        throw new IllegalStateException("run cannot be cancelled until its execution ownership is known");
      }
      cancelRunTree(tenantId, runId, requestedAt);
      return target.status();
    });
  }

  private void cancelRunTree(UUID tenantId, UUID rootRunId, Instant requestedAt) {
    var targets = jdbc.query("""
        with recursive tree(id,depth) as (
          select id,0 from runs where tenant_id = ? and id = ?
          union all
          select child.id,parent.depth + 1
            from runs child join tree parent on child.parent_run_id = parent.id
           where child.tenant_id = ?
        )
        select r.id,r.status,r.session_id,r.current_attempt_id,d.worker_id,
               d.worker_incarnation_id
          from tree join runs r on r.tenant_id = ? and r.id = tree.id
          left join run_dispatches d on d.tenant_id = r.tenant_id and d.run_id = r.id
            and d.attempt_id = r.current_attempt_id
         order by tree.depth desc,r.id
         for update of r
        """, (row, rowNumber) -> new TreeCancellationTarget(
            row.getObject("id", UUID.class),
            RunStatus.valueOf(row.getString("status").toUpperCase()),
            row.getObject("session_id", UUID.class),
            row.getObject("current_attempt_id", UUID.class),
            row.getObject("worker_id", UUID.class),
            row.getObject("worker_incarnation_id", UUID.class)),
        tenantId, rootRunId, tenantId, tenantId);
    for (var target : targets) {
      if (isTerminal(target.status())) {
        continue;
      }
      if (target.status() == RunStatus.QUEUED && target.attemptId() == null) {
        cancelUndispatchedRun(tenantId, target.runId(), target.sessionId(), requestedAt);
        continue;
      }
      if (!List.of(
              RunStatus.QUEUED, RunStatus.RUNNING,
              RunStatus.WAITING_APPROVAL, RunStatus.SUSPENDED)
              .contains(target.status())
          || target.attemptId() == null || target.workerId() == null
          || target.workerIncarnationId() == null) {
        throw new IllegalStateException(
            "subagent run cannot be cancelled until its execution ownership is known");
      }
      enqueueCancellation(
          tenantId, target.runId(), new CancellationTarget(
              target.status(), target.sessionId(), target.attemptId(), target.workerId(),
              target.workerIncarnationId()), requestedAt);
    }
    jdbc.update("""
        with recursive tree(id) as (
          select id from runs where tenant_id = ? and id = ?
          union all
          select child.id from runs child join tree parent on child.parent_run_id = parent.id
           where child.tenant_id = ?
        )
        update subagent_calls
           set state = 'cancelled', updated_at = clock_timestamp()
         where tenant_id = ? and state in (
           'awaiting_checkpoint','child_queued','result_ready')
           and (parent_run_id in (select id from tree)
             or child_run_id in (select id from tree))
        """, tenantId, rootRunId, tenantId, tenantId);
    jdbc.update("""
        with recursive tree(id) as (
          select id from runs where tenant_id = ? and id = ?
          union all
          select child.id from runs child join tree parent on child.parent_run_id = parent.id
           where child.tenant_id = ?
        )
        update run_steering_commands
           set state = 'cancelled', updated_at = clock_timestamp()
         where tenant_id = ? and state = 'pending' and run_id in (select id from tree)
        """, tenantId, rootRunId, tenantId, tenantId);
  }

  private void cancelUndispatchedRun(
      UUID tenantId, UUID runId, UUID sessionId, Instant requestedAt) {
    var eventId = UUID.randomUUID();
    var attemptId = UUID.randomUUID();
    var traceId = UUID.randomUUID();
    var payload = "{\"status\":\"cancelled\"}";
    jdbc.update("""
        insert into run_events (
          tenant_id,event_id,run_id,session_id,sequence,schema_version,attempt_id,
          occurred_at,trace_id,type,payload,digest)
        values (?,?,?,?,1,1,?,?,?,'run.cancelled',?::jsonb,?)
        """, tenantId, eventId, runId, sessionId, attemptId, Timestamp.from(requestedAt),
        traceId.toString(), payload, sha256(payload));
    jdbc.update("""
        update runs
           set status = 'cancelled', last_sequence = 1, finished_at = ?,
               updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and status = 'queued' and current_attempt_id is null
        """, Timestamp.from(requestedAt), tenantId, runId);
  }

  private void enqueueCancellation(
      UUID tenantId, UUID runId, CancellationTarget target, Instant requestedAt) {
    var existing = jdbc.queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.cancellation.requested'
           and (payload->>'attempt_id')::uuid = ?
        """, Integer.class, tenantId, runId, target.attemptId());
    if (existing != null && existing > 0) {
      return;
    }
    var messageId = UUID.randomUUID();
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        values (?,?,'run',?,'run.cancellation.requested',jsonb_build_object(
          'schema_version',2,
          'message_id',?,
          'tenant_id',?,
          'run_id',?,
          'attempt_id',?,
          'worker_id',?,
          'worker_incarnation_id',?,
          'issued_at',?,
          'expires_at',?,
          'reason','user_requested'))
        """, tenantId, messageId, runId, messageId.toString(), tenantId.toString(),
        runId.toString(), target.attemptId().toString(), target.workerId().toString(),
        target.workerIncarnationId().toString(), requestedAt.toString(),
        requestedAt.plusSeconds(30).toString());
  }

  @Override
  public RunSteeringResult requestSteering(
      UUID tenantId,
      UUID applicationId,
      UUID runId,
      String idempotencyKey,
      String input,
      Instant requestedAt) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      var inputDigest = sha256(input);
      var existing = jdbc.query("""
          select steering_id,input,input_digest,state
            from run_steering_commands
           where tenant_id = ? and application_id = ? and run_id = ? and idempotency_key = ?
           for update
          """, (row, rowNumber) -> new ExistingSteering(
              row.getObject("steering_id", UUID.class),
              row.getString("input"),
              row.getString("input_digest"),
              row.getString("state")), tenantId, applicationId, runId, idempotencyKey);
      if (!existing.isEmpty()) {
        var command = existing.getFirst();
        if (!command.input().equals(input) || !command.inputDigest().equals(inputDigest)) {
          throw new RunSteeringConflict(runId);
        }
        return new RunSteeringResult(runId, command.steeringId(), command.state());
      }

      var targets = jdbc.query("""
          select r.current_attempt_id,d.worker_id,d.worker_incarnation_id,d.lease_expires_at,
                 exists (
                   select 1 from approvals a
                    where a.tenant_id = r.tenant_id and a.run_id = r.id
                      and a.attempt_id = r.current_attempt_id and a.status = 'pending')
                   as pending_approval,
                 exists (
                   select 1 from tool_executions t
                    where t.tenant_id = r.tenant_id and t.run_id = r.id
                      and t.attempt_id = r.current_attempt_id and t.state <> 'completed')
                   as pending_tool,
                 exists (
                   select 1 from subagent_calls s
                    where s.tenant_id = r.tenant_id and s.parent_run_id = r.id
                      and s.parent_attempt_id = r.current_attempt_id
                      and s.state not in ('delivered','cancelled'))
                   as pending_subagent
            from runs r
            join run_dispatches d
              on d.tenant_id = r.tenant_id and d.run_id = r.id
             and d.attempt_id = r.current_attempt_id and d.state = 'accepted'
           where r.tenant_id = ? and r.application_id = ? and r.id = ? and r.status = 'running'
           for update of r,d
          """, (row, rowNumber) -> new SteeringTarget(
              row.getObject("current_attempt_id", UUID.class),
              row.getObject("worker_id", UUID.class),
              row.getObject("worker_incarnation_id", UUID.class),
              row.getTimestamp("lease_expires_at").toInstant(),
              row.getBoolean("pending_approval"),
              row.getBoolean("pending_tool"),
              row.getBoolean("pending_subagent")), tenantId, applicationId, runId);
      if (targets.isEmpty()) {
        var exists = jdbc.queryForObject("""
            select count(*) from runs where tenant_id = ? and application_id = ? and id = ?
            """, Integer.class, tenantId, applicationId, runId);
        if (exists == null || exists == 0) {
          throw new RunNotFound(runId);
        }
        throw new RunSteeringNotAllowed(runId);
      }
      var target = targets.getFirst();
      if (!target.leaseExpiresAt().isAfter(requestedAt)
          || target.pendingApproval()
          || target.pendingTool()
          || target.pendingSubagent()) {
        throw new RunSteeringNotAllowed(runId);
      }
      var latestRequestedAt = jdbc.query("""
          select requested_at from run_steering_commands
           where tenant_id = ? and run_id = ?
           order by requested_at desc
           limit 1
          """, (row, rowNumber) -> row.getTimestamp("requested_at").toInstant(),
          tenantId, runId);
      if (!latestRequestedAt.isEmpty()) {
        var nextAcceptedAt = latestRequestedAt.getFirst().plusSeconds(2);
        if (requestedAt.isBefore(nextAcceptedAt)) {
          throw new RunSteeringRateLimited(
              runId, Duration.between(requestedAt, nextAcceptedAt));
        }
      }
      var pending = jdbc.queryForObject("""
          select count(*) from run_steering_commands
           where tenant_id = ? and run_id = ? and state = 'pending'
          """, Integer.class, tenantId, runId);
      if (pending != null && pending > 0) {
        throw new RunSteeringNotAllowed(runId);
      }

      var steeringId = UUID.randomUUID();
      var messageId = UUID.randomUUID();
      var expiresAt = requestedAt.plusSeconds(30);
      jdbc.update("""
          insert into run_steering_commands (
            tenant_id,application_id,run_id,steering_id,idempotency_key,input,input_digest,
            attempt_id,worker_id,worker_incarnation_id,requested_at,issued_at,expires_at)
          values (?,?,?,?,?,?,?,?,?,?,?,?,?)
          """, tenantId, applicationId, runId, steeringId, idempotencyKey, input, inputDigest,
          target.attemptId(), target.workerId(), target.workerIncarnationId(),
          Timestamp.from(requestedAt), Timestamp.from(requestedAt), Timestamp.from(expiresAt));
      jdbc.update("""
          insert into outbox_events (
            tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
          values (?,?,'run',?,'run.steering.requested',jsonb_build_object(
            'schema_version',1,
            'message_id',?,
            'steering_id',?,
            'tenant_id',?,
            'run_id',?,
            'attempt_id',?,
            'worker_id',?,
            'worker_incarnation_id',?,
            'input',?,
            'input_digest',?,
            'issued_at',?,
            'expires_at',?))
          """, tenantId, messageId, runId, messageId.toString(), steeringId.toString(),
          tenantId.toString(), runId.toString(), target.attemptId().toString(),
          target.workerId().toString(), target.workerIncarnationId().toString(), input,
          inputDigest, requestedAt.toString(), expiresAt.toString());
      return new RunSteeringResult(runId, steeringId, "pending");
    });
  }

  private boolean isTerminal(RunStatus status) {
    return switch (status) {
      case SUCCEEDED, FAILED, CANCELLED, TIMED_OUT, INDETERMINATE -> true;
      default -> false;
    };
  }

  private String sha256(String value) {
    try {
      return HexFormat.of().formatHex(
          MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8)));
    } catch (NoSuchAlgorithmException impossible) {
      throw new IllegalStateException("SHA-256 is unavailable", impossible);
    }
  }

  @Override
  public List<RunSummary> findRecent(UUID tenantId, UUID applicationId, int limit) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      return jdbc.query("""
          select r.id, w.name as workspace_name, a.name as agent_name, r.status,
                 r.max_tokens, r.max_cost_cents, r.max_duration_seconds, r.created_at
            from runs r
            join workspaces w on w.tenant_id = r.tenant_id and w.id = r.workspace_id
            join agent_versions av on av.tenant_id = r.tenant_id and av.id = r.agent_version_id
            join agents a on a.tenant_id = av.tenant_id and a.id = av.agent_id
           where r.tenant_id = ? and r.application_id = ?
           order by r.created_at desc
           limit ?
          """, (row, rowNumber) -> new RunSummary(
              row.getObject("id", UUID.class),
              row.getString("workspace_name"),
              row.getString("agent_name"),
              RunStatus.valueOf(row.getString("status").toUpperCase()),
              row.getLong("max_tokens"),
              row.getLong("max_cost_cents"),
              row.getLong("max_duration_seconds"),
              row.getTimestamp("created_at").toInstant()), tenantId, applicationId, limit);
    });
  }

  /**
   * Refuses admission when the tenant is already at its concurrent Run limit.
   *
   * <p>Takes the tenant's quota row {@code for update} before counting.
   * Counting and then inserting without the lock lets two concurrent requests
   * both see room and both insert, which is the failure this exists to prevent
   * -- the same reason subagent admission locks the parent Run row.
   *
   * <p>An idempotent retry is not charged: if the key already names a Run, the
   * caller is asking about capacity it already holds, and refusing would give a
   * quota error for the client's own Run.
   */
  private void admitWithinTenantQuota(UUID tenantId, UUID applicationId, String idempotencyKey) {
    var alreadyAdmitted = jdbc.queryForObject(
        "select count(*) from runs where tenant_id = ? and application_id = ? and idempotency_key = ?",
        Integer.class, tenantId, applicationId, idempotencyKey);
    if (alreadyAdmitted != null && alreadyAdmitted > 0) {
      return;
    }
    var limits = jdbc.query(
        "select max_active_runs from tenant_run_quotas where tenant_id = ? for update",
        (row, rowNumber) -> row.getInt("max_active_runs"), tenantId);
    if (limits.isEmpty()) {
      // No row means no configured limit for this tenant. Inserting the default
      // here rather than treating absence as unlimited keeps the lock target
      // present for every subsequent admission.
      jdbc.update(
          "insert into tenant_run_quotas (tenant_id, max_active_runs) values (?, ?)"
              + " on conflict (tenant_id) do nothing",
          tenantId, DEFAULT_MAX_ACTIVE_RUNS);
      limits = jdbc.query(
          "select max_active_runs from tenant_run_quotas where tenant_id = ? for update",
          (row, rowNumber) -> row.getInt("max_active_runs"), tenantId);
    }
    var limit = limits.isEmpty() ? DEFAULT_MAX_ACTIVE_RUNS : limits.get(0);
    var active = jdbc.queryForObject(
        "select count(*) from runs where tenant_id = ? and status in "
            + "('queued','running','waiting_approval','suspended')",
        Integer.class, tenantId);
    if (active != null && active >= limit) {
      throw new TenantQuotaExceeded(
          "tenant is at its concurrent run limit of " + limit,
          TENANT_QUOTA_RETRY_AFTER_SECONDS);
    }
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject("select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }

  private Optional<Run> find(UUID tenantId, UUID applicationId, String idempotencyKey) {
    return jdbc.query(SELECT_BY_KEY, this::mapRun, tenantId, applicationId, idempotencyKey)
        .stream().findFirst();
  }

  private Run mapRun(ResultSet row, int rowNumber) throws SQLException {
    return new Run(
        row.getObject("id", UUID.class),
        row.getObject("tenant_id", UUID.class),
        row.getObject("session_id", UUID.class),
        row.getObject("agent_version_id", UUID.class),
        row.getObject("workspace_id", UUID.class),
        row.getObject("model_policy_id", UUID.class),
        row.getString("idempotency_key"),
        row.getString("input"),
        RunStatus.valueOf(row.getString("status").toUpperCase()),
        row.getLong("max_tokens"),
        row.getLong("max_cost_cents"),
        row.getLong("max_duration_seconds"),
        row.getTimestamp("created_at").toInstant());
  }

  private record CancellationTarget(
      RunStatus status,
      UUID sessionId,
      UUID attemptId,
      UUID workerId,
      UUID workerIncarnationId) {}

  private record ExistingSteering(
      UUID steeringId, String input, String inputDigest, String state) {}

  private record SteeringTarget(
      UUID attemptId,
      UUID workerId,
      UUID workerIncarnationId,
      Instant leaseExpiresAt,
      boolean pendingApproval,
      boolean pendingTool,
      boolean pendingSubagent) {}

  private record TreeCancellationTarget(
      UUID runId,
      RunStatus status,
      UUID sessionId,
      UUID attemptId,
      UUID workerId,
      UUID workerIncarnationId) {}
}
