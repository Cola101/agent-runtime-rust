package com.agentplatform.control.scheduler;

import com.agentplatform.control.approval.ToolApprovalScope;
import com.agentplatform.control.identity.WorkloadIdentityClaims;
import com.agentplatform.control.identity.WorkloadToken;
import com.agentplatform.control.identity.WorkloadTokenIssuer;
import com.agentplatform.control.persistence.JdbcSubagentAdmissionRepository;
import com.agentplatform.control.run.SpawnSubagentCommand;
import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Timestamp;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.time.Duration;
import java.time.Instant;
import java.util.HexFormat;
import java.util.Base64;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.UUID;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
@ConditionalOnProperty(prefix = "agent.runtime.scheduler", name = "enabled", havingValue = "true")
public class JdbcSchedulerRepository implements RecoveryMetricsSource {
  private static final ObjectMapper JSON = new ObjectMapper();
  private static final int EXECUTION_SCHEMA_VERSION = 7;
  /** Redispatch bound for an assignment no Worker has ever accepted. */
  private static final int MAX_UNACCEPTED_DISPATCH_ATTEMPTS = 5;
  private static final int SUBAGENT_RESULT_TEXT_MAX_BYTES = 240 * 1024;
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;
  private final WorkloadTokenIssuer workloadTokenIssuer;
  private final JdbcSubagentAdmissionRepository subagentAdmission;

  public JdbcSchedulerRepository(
      JdbcTemplate jdbc, TransactionTemplate transactions, WorkloadTokenIssuer workloadTokenIssuer) {
    this.jdbc = jdbc;
    this.transactions = transactions;
    this.workloadTokenIssuer = workloadTokenIssuer;
    this.subagentAdmission = new JdbcSubagentAdmissionRepository(jdbc, transactions);
  }

  public void recordHeartbeat(WorkerHeartbeatMessage heartbeat) {
    recordHeartbeat(heartbeat, Duration.ofSeconds(30));
  }

  public void recordHeartbeat(WorkerHeartbeatMessage heartbeat, Duration leaseDuration) {
    validateDuration(leaseDuration, "lease duration", Duration.ofMinutes(5));
    transactions.executeWithoutResult(status -> {
      var seeded = jdbc.update("""
        insert into runtime_workers (
          id, current_incarnation_id, placements, capacity, active_runs,
          runtime_version, last_heartbeat, accepting_work, draining_since, drain_deadline)
        values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        on conflict (id) do nothing
        """, heartbeat.workerId(), heartbeat.incarnationId(),
          heartbeat.placements().toArray(String[]::new),
          heartbeat.capacity(), heartbeat.activeRuns(), heartbeat.runtimeVersion(),
          Timestamp.from(heartbeat.occurredAt()), heartbeat.acceptingWork(),
          timestamp(heartbeat.drainingSince()), timestamp(heartbeat.drainDeadline()));
      jdbc.update("""
          insert into runtime_worker_incarnations (
            worker_id,incarnation_id,placements,capacity,active_runs,runtime_version,
            last_heartbeat,last_heartbeat_received_at,accepting_work,draining_since,
            drain_deadline,created_at)
          values (?,?,?,?,?,?,?,clock_timestamp(),?,?,?,clock_timestamp())
          on conflict (worker_id,incarnation_id) do update
             set placements = excluded.placements,
                 capacity = excluded.capacity,
                 active_runs = excluded.active_runs,
                 runtime_version = excluded.runtime_version,
                 last_heartbeat = excluded.last_heartbeat,
                 last_heartbeat_received_at = clock_timestamp(),
                 accepting_work = runtime_worker_incarnations.accepting_work
                                  and excluded.accepting_work,
                 draining_since = case
                   when runtime_worker_incarnations.accepting_work
                     then excluded.draining_since
                   else runtime_worker_incarnations.draining_since
                 end,
                 drain_deadline = case
                   when runtime_worker_incarnations.accepting_work
                     then excluded.drain_deadline
                   else runtime_worker_incarnations.drain_deadline
                 end,
                 updated_at = clock_timestamp()
           where runtime_worker_incarnations.last_heartbeat <= excluded.last_heartbeat
          """, heartbeat.workerId(), heartbeat.incarnationId(),
          heartbeat.placements().toArray(String[]::new), heartbeat.capacity(),
          heartbeat.activeRuns(), heartbeat.runtimeVersion(), Timestamp.from(heartbeat.occurredAt()),
          heartbeat.acceptingWork(), timestamp(heartbeat.drainingSince()),
          timestamp(heartbeat.drainDeadline()));
      var refreshed = jdbc.update("""
          update runtime_workers
             set placements = ?, capacity = ?, active_runs = ?, runtime_version = ?,
                 last_heartbeat = ?, accepting_work = accepting_work and ?,
                 draining_since = case when accepting_work then ? else draining_since end,
                 drain_deadline = case when accepting_work then ? else drain_deadline end,
                 updated_at = clock_timestamp()
           where id = ? and current_incarnation_id = ? and last_heartbeat <= ?
          """, heartbeat.placements().toArray(String[]::new), heartbeat.capacity(),
          heartbeat.activeRuns(), heartbeat.runtimeVersion(), Timestamp.from(heartbeat.occurredAt()),
          heartbeat.acceptingWork(), timestamp(heartbeat.drainingSince()),
          timestamp(heartbeat.drainDeadline()), heartbeat.workerId(), heartbeat.incarnationId(),
          Timestamp.from(heartbeat.occurredAt()));
      var switched = jdbc.update("""
          update runtime_workers w
             set current_incarnation_id = candidate.incarnation_id,
                 placements = candidate.placements,
                 capacity = candidate.capacity,
                 active_runs = candidate.active_runs,
                 runtime_version = candidate.runtime_version,
                 last_heartbeat = candidate.last_heartbeat,
                 accepting_work = candidate.accepting_work,
                 draining_since = candidate.draining_since,
                 drain_deadline = candidate.drain_deadline,
                 updated_at = clock_timestamp()
            from runtime_worker_incarnations candidate,
                 runtime_worker_incarnations current
           where w.id = ?
             and candidate.worker_id = w.id and candidate.incarnation_id = ?
             and current.worker_id = w.id
             and current.incarnation_id = w.current_incarnation_id
             and candidate.created_at > current.created_at
          """, heartbeat.workerId(), heartbeat.incarnationId());
      if (seeded + refreshed + switched == 0) {
        return;
      }
      heartbeat.activeAssignments().forEach(assignment ->
          renewAssignment(
              heartbeat.workerId(), heartbeat.incarnationId(), assignment, leaseDuration));
    });
  }

  private void renewAssignment(
      UUID workerId,
      UUID workerIncarnationId,
      ActiveAssignmentMessage assignment,
      Duration leaseDuration) {
    setTenant(assignment.tenantId());
    var valid = jdbc.query("""
        select r.model_policy_id
          from run_dispatches d
          join runs r on r.tenant_id = d.tenant_id and r.id = d.run_id
          join workspace_leases l
            on l.tenant_id = r.tenant_id and l.workspace_id = r.workspace_id
         where d.tenant_id = ? and d.run_id = ? and d.attempt_id = ?
           and d.worker_id = ? and d.worker_incarnation_id = ?
           and d.owner_epoch = ? and d.fencing_token = ?
           and r.workspace_id = ? and r.current_attempt_id = d.attempt_id
           and d.state = 'accepted'
           and d.lease_expires_at > clock_timestamp()
           and l.owner_id = d.worker_id and l.owner_epoch = d.owner_epoch
           and l.fencing_token = d.fencing_token and l.expires_at > clock_timestamp()
        """, (row, rowNumber) -> new RenewableAssignment(
            row.getObject("model_policy_id", UUID.class)),
        assignment.tenantId(), assignment.runId(), assignment.attemptId(),
        workerId, workerIncarnationId, assignment.ownerEpoch(), assignment.fencingToken(),
        assignment.workspaceId());
    if (valid.size() != 1) {
      return;
    }
    jdbc.update("""
        update run_dispatches
           set lease_expires_at = clock_timestamp() + (? * interval '1 millisecond'),
               updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, leaseDuration.toMillis(), assignment.tenantId(), assignment.runId(),
        assignment.attemptId());
    jdbc.update("""
        update workspace_leases
           set expires_at = clock_timestamp() + (? * interval '1 millisecond'),
               updated_at = clock_timestamp()
         where tenant_id = ? and workspace_id = ? and owner_id = ?
           and owner_epoch = ? and fencing_token = ?
        """, leaseDuration.toMillis(), assignment.tenantId(), assignment.workspaceId(), workerId,
        assignment.ownerEpoch(), assignment.fencingToken());
    var renewals = jdbc.query("""
        update run_dispatches
           set workload_identity_generation = workload_identity_generation + 1,
               workload_identity_expires_at = lease_expires_at,
               updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and attempt_id = ?
           and worker_id = ? and worker_incarnation_id = ?
           and owner_epoch = ? and fencing_token = ? and state = 'accepted'
           and workload_identity_expires_at
               <= clock_timestamp() + (? * interval '1 millisecond')
        returning workload_identity_generation,workload_identity_expires_at
        """, (row, rowNumber) -> new IdentityRenewal(
            row.getLong("workload_identity_generation"),
            row.getTimestamp("workload_identity_expires_at").toInstant()),
        assignment.tenantId(), assignment.runId(), assignment.attemptId(), workerId,
        workerIncarnationId, assignment.ownerEpoch(), assignment.fencingToken(),
        Math.max(1, leaseDuration.toMillis() / 2));
    if (!renewals.isEmpty()) {
      insertIdentityRenewalOutbox(
          workerId, workerIncarnationId, assignment, valid.getFirst().modelPolicyId(),
          renewals.getFirst());
    }
  }

  private void insertIdentityRenewalOutbox(
      UUID workerId,
      UUID workerIncarnationId,
      ActiveAssignmentMessage assignment,
      UUID modelPolicyId,
      IdentityRenewal renewal) {
    var messageId = UUID.randomUUID();
    var issuedAt = Instant.now();
    var modelPolicySnapshot = modelPolicySnapshot(assignment.tenantId(), modelPolicyId);
    var workloadToken = workloadTokenIssuer.issue(new WorkloadIdentityClaims(
        assignment.tenantId(), assignment.runId(), assignment.attemptId(), workerId,
        workerIncarnationId, modelPolicyId, modelPolicySnapshot.digest(), issuedAt,
        renewal.expiresAt()));
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        values (?,?,'run',?,'workload.identity.renewed',jsonb_build_object(
          'schema_version',1,
          'message_id',?,
          'tenant_id',?,
          'run_id',?,
          'attempt_id',?,
          'worker_id',?,
          'worker_incarnation_id',?,
          'owner_epoch',?,
          'fencing_token',?,
          'generation',?,
          'issued_at',?,
          'lease_expires_at',?,
          'workload_token',?
        ))
        """, assignment.tenantId(), messageId, assignment.runId(), messageId.toString(),
        assignment.tenantId().toString(), assignment.runId().toString(),
        assignment.attemptId().toString(), workerId.toString(), workerIncarnationId.toString(),
        assignment.ownerEpoch(), assignment.fencingToken().toString(), renewal.generation(),
        issuedAt.toString(), renewal.expiresAt().toString(), workloadToken.value());
  }

  public ReconcileResult reconcileExpired() {
    return reconcileExpired(Duration.ofSeconds(30), Duration.ofSeconds(15));
  }

  public ReconcileResult reconcileExpired(
      Duration leaseDuration, Duration heartbeatFreshness) {
    validateDuration(leaseDuration, "lease duration", Duration.ofMinutes(5));
    validateDuration(heartbeatFreshness, "heartbeat freshness", Duration.ofMinutes(5));
    var readySubagents = jdbc.query("""
        select tenant_id,parent_run_id,tool_call_id
          from subagent_calls
         where state = 'result_ready' and delivery_attempt_id is null
         order by updated_at
         limit 100
        """, (row, rowNumber) -> new SubagentResultKey(
            row.getObject("tenant_id", UUID.class),
            row.getObject("parent_run_id", UUID.class),
            row.getString("tool_call_id")));
    var recovered = 0;
    for (var key : readySubagents) {
      if (Boolean.TRUE.equals(transactions.execute(status ->
          resumeSubagentInTransaction(key, leaseDuration, heartbeatFreshness)))) {
        recovered++;
      }
    }
    var expired = jdbc.query("""
        select tenant_id,run_id,attempt_id
          from run_dispatches
         where state in ('requested', 'accepted') and lease_expires_at <= clock_timestamp()
         order by lease_expires_at
         limit 100
        """, (row, rowNumber) -> new DispatchKey(
            row.getObject("tenant_id", UUID.class),
            row.getObject("run_id", UUID.class),
            row.getObject("attempt_id", UUID.class)));
    var requeued = 0;
    var indeterminate = 0;
    var failed = 0;
    for (var key : expired) {
      var outcome = transactions.execute(status ->
          reconcileInTransaction(key, leaseDuration, heartbeatFreshness));
      if (outcome == ReconcileOutcome.REQUEUED) {
        requeued++;
      } else if (outcome == ReconcileOutcome.RECOVERED) {
        recovered++;
      } else if (outcome == ReconcileOutcome.INDETERMINATE) {
        indeterminate++;
      } else if (outcome == ReconcileOutcome.FAILED) {
        failed++;
      }
    }
    return new ReconcileResult(requeued, recovered, indeterminate, failed);
  }

  private ReconcileOutcome reconcileInTransaction(
      DispatchKey key, Duration leaseDuration, Duration heartbeatFreshness) {
    setTenant(key.tenantId());
    var expired = jdbc.query("""
        select d.state,d.worker_id,d.worker_incarnation_id,d.owner_epoch,d.fencing_token,
               i.last_heartbeat_received_at as last_confirmed_healthy_at,
               r.session_id,r.workspace_id,
               r.agent_version_id,r.model_policy_id,r.input,r.status,r.placement,r.last_sequence,
               r.max_tokens,r.max_cost_cents,r.max_duration_seconds,
               coalesce(r.root_run_id,r.id) as root_run_id,r.parent_run_id,r.delegation_id,
               r.subagent_depth,r.agent_role,
               case when r.subagent_depth = 0
                 then coalesce(av.spec->>'instructions','')
                 else concat(coalesce(av.spec->>'instructions',''),
                   E'\n\n[Subagent role ',r.agent_role,E']\n',sr.role->>'instructions')
               end as agent_instructions,
               array(select jsonb_array_elements_text(
                 case when r.subagent_depth = 0
                   then coalesce(av.spec->'delegated_scopes','[]'::jsonb)
                   else coalesce(sr.role->'delegated_scopes','[]'::jsonb)
                 end) order by 1) delegated_scopes
          from run_dispatches d
          join runs r on r.tenant_id = d.tenant_id and r.id = d.run_id
          join agent_versions av on av.tenant_id = r.tenant_id and av.id = r.agent_version_id
          left join lateral (
            select role_value as role from jsonb_array_elements(
              coalesce(av.spec->'subagent_roles','[]'::jsonb)) as roles(role_value)
             where role_value->>'name' = r.agent_role limit 1
          ) sr on r.subagent_depth > 0
          join runtime_worker_incarnations i
            on i.worker_id = d.worker_id and i.incarnation_id = d.worker_incarnation_id
         where d.tenant_id = ? and d.run_id = ? and d.attempt_id = ?
           and d.state in ('requested', 'accepted')
           and d.lease_expires_at <= clock_timestamp()
           and (r.subagent_depth = 0 or sr.role is not null)
         for update of d,r
        """, (row, rowNumber) -> new ExpiredDispatch(
            row.getString("state"),
            row.getObject("worker_id", UUID.class),
            row.getObject("worker_incarnation_id", UUID.class),
            row.getTimestamp("last_confirmed_healthy_at").toInstant(),
            row.getLong("owner_epoch"),
            row.getObject("fencing_token", UUID.class),
            row.getObject("session_id", UUID.class),
            row.getObject("workspace_id", UUID.class),
            row.getObject("agent_version_id", UUID.class),
            row.getObject("model_policy_id", UUID.class),
            row.getString("input"),
            row.getString("status"),
            row.getString("placement"),
            row.getLong("last_sequence"),
            stringList(row, "delegated_scopes"),
            row.getString("agent_instructions"),
            row.getObject("root_run_id", UUID.class),
            row.getObject("parent_run_id", UUID.class),
            row.getObject("delegation_id", UUID.class),
            row.getInt("subagent_depth"),
            row.getString("agent_role"),
            row.getLong("max_tokens"),
            row.getLong("max_cost_cents"),
            row.getLong("max_duration_seconds")),
        key.tenantId(), key.runId(), key.attemptId());
    if (expired.isEmpty()) {
      return ReconcileOutcome.NONE;
    }
    var dispatch = expired.getFirst();
    var safeCheckpoint = "accepted".equals(dispatch.state())
        ? findSafeCheckpoint(key, dispatch) : Optional.<StoredCheckpoint>empty();
    var recoveryWorker = safeCheckpoint.isPresent()
        ? findRecoveryWorker(dispatch, heartbeatFreshness, false) : Optional.<WorkerTarget>empty();
    var incidentId = "accepted".equals(dispatch.state())
        ? Optional.of(ensureRecoveryIncident(key, dispatch)) : Optional.<UUID>empty();
    if (safeCheckpoint.isPresent() && recoveryWorker.isEmpty()) {
      markRecoveryWaitingForCapacity(key, incidentId.orElseThrow());
      return ReconcileOutcome.NONE;
    }
    jdbc.update("""
        update run_dispatches set state = 'lost', updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, key.tenantId(), key.runId(), key.attemptId());
    jdbc.update("""
        update runtime_workers
           set active_runs = greatest(active_runs - 1, 0), updated_at = clock_timestamp()
         where id = ? and current_incarnation_id = ?
        """, dispatch.workerId(), dispatch.workerIncarnationId());
    jdbc.update("""
        update runtime_worker_incarnations
           set active_runs = greatest(active_runs - 1, 0), updated_at = clock_timestamp()
         where worker_id = ? and incarnation_id = ?
        """, dispatch.workerId(), dispatch.workerIncarnationId());
    jdbc.update("""
        update workspaces set state = 'ready', updated_at = clock_timestamp()
         where tenant_id = ? and id = ?
           and not exists (
             select 1 from workspace_leases l
              where l.tenant_id = workspaces.tenant_id and l.workspace_id = workspaces.id
                and l.expires_at > clock_timestamp())
        """, key.tenantId(), dispatch.workspaceId());

    if ("requested".equals(dispatch.state())) {
      // A Worker that refuses an assignment terminates the transport message and
      // reports nothing, so the control plane only ever sees a dispatch that was
      // never accepted. Without a bound the same poisoned assignment is
      // redispatched forever and each redispatch renews the workspace lease, so
      // the workspace never returns to 'ready' and every later Run in it is
      // refused. A Run that has emitted no event has no side effects to
      // preserve, so failing it closed is safe.
      if (dispatch.lastSequence() == 0
          && dispatchAttempts(key) >= MAX_UNACCEPTED_DISPATCH_ATTEMPTS) {
        failNeverAcceptedRun(key, dispatch);
        return ReconcileOutcome.FAILED;
      }
      jdbc.update("""
          update runs set current_attempt_id = null, updated_at = clock_timestamp()
           where tenant_id = ? and id = ? and current_attempt_id = ? and status = 'queued'
          """, key.tenantId(), key.runId(), key.attemptId());
      insertRunQueuedOutbox(key.tenantId(), key.runId());
      return ReconcileOutcome.REQUEUED;
    }

    if (safeCheckpoint.isPresent()) {
      dispatchRecovery(
          key, dispatch, safeCheckpoint.orElseThrow(), recoveryWorker.orElseThrow(),
          leaseDuration, incidentId.orElseThrow());
      return ReconcileOutcome.RECOVERED;
    }

    var sequence = dispatch.lastSequence() + 1;
    var eventId = UUID.randomUUID();
    var traceId = UUID.randomUUID();
    var occurredAt = Instant.now();
    var payload = indeterminatePayload(key);
    jdbc.update("""
        insert into run_events (
          tenant_id,event_id,run_id,session_id,sequence,schema_version,attempt_id,
          occurred_at,trace_id,type,payload,digest)
        values (?,?,?,?,?,1,?,?,?,'run.indeterminate',?::jsonb,?)
        """, key.tenantId(), eventId, key.runId(), dispatch.sessionId(), sequence,
        key.attemptId(), Timestamp.from(occurredAt), traceId.toString(), payload, sha256(payload));
    jdbc.update("""
        update runs
           set status = 'indeterminate', current_attempt_id = null, last_sequence = ?,
               finished_at = ?, updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and current_attempt_id = ?
           and status in ('running','waiting_approval','suspended')
        """, sequence, Timestamp.from(occurredAt), key.tenantId(), key.runId(), key.attemptId());
    rejectPendingSteering(key.tenantId(), key.runId(), "recovery_indeterminate");
    resolveRecoveryIncident(key, incidentId.orElseThrow(), "indeterminate", occurredAt);
    return ReconcileOutcome.INDETERMINATE;
  }

  private boolean resumeSubagentInTransaction(
      SubagentResultKey key, Duration leaseDuration, Duration heartbeatFreshness) {
    setTenant(key.tenantId());
    var results = jdbc.query("""
        select s.parent_attempt_id,s.delegation_id,s.binding_digest,s.child_run_id,
               s.child_terminal_event_id,s.terminal_status,s.result::text result,
               s.result_digest,s.result_is_error,
               d.worker_id,d.worker_incarnation_id,d.owner_epoch,d.fencing_token,
               i.last_heartbeat_received_at as last_confirmed_healthy_at,
               r.session_id,r.workspace_id,r.agent_version_id,r.model_policy_id,r.input,
               r.status,r.placement,r.last_sequence,r.max_tokens,r.max_cost_cents,
               r.max_duration_seconds,coalesce(r.root_run_id,r.id) as root_run_id,
               r.parent_run_id,r.delegation_id parent_delegation_id,r.subagent_depth,r.agent_role,
               case when r.subagent_depth = 0
                 then coalesce(av.spec->>'instructions','')
                 else concat(coalesce(av.spec->>'instructions',''),
                   E'\n\n[Subagent role ',r.agent_role,E']\n',sr.role->>'instructions')
               end as agent_instructions,
               array(select jsonb_array_elements_text(
                 case when r.subagent_depth = 0
                   then coalesce(av.spec->'delegated_scopes','[]'::jsonb)
                   else coalesce(sr.role->'delegated_scopes','[]'::jsonb)
                 end) order by 1) delegated_scopes,
               c.checkpoint_id,c.kernel_digest,c.tool_catalog_digest,c.payload,c.payload_ref,
               c.payload_encoding,c.payload_digest,c.stored_payload_digest,
               c.uncompressed_size,c.stored_size,c.created_at
          from subagent_calls s
          join runs r on r.tenant_id = s.tenant_id and r.id = s.parent_run_id
          join run_dispatches d on d.tenant_id = s.tenant_id
            and d.run_id = s.parent_run_id and d.attempt_id = s.parent_attempt_id
          join runtime_worker_incarnations i on i.worker_id = d.worker_id
            and i.incarnation_id = d.worker_incarnation_id
          join agent_versions av on av.tenant_id = r.tenant_id and av.id = r.agent_version_id
          left join lateral (
            select role_value as role from jsonb_array_elements(
              coalesce(av.spec->'subagent_roles','[]'::jsonb)) as roles(role_value)
             where role_value->>'name' = r.agent_role limit 1
          ) sr on r.subagent_depth > 0
          join run_checkpoints c on c.tenant_id = s.tenant_id
            and c.checkpoint_id = s.parent_checkpoint_id
         where s.tenant_id = ? and s.parent_run_id = ? and s.tool_call_id = ?
           and s.state = 'result_ready' and s.delivery_attempt_id is null
           and r.status = 'suspended' and r.current_attempt_id = s.parent_attempt_id
           and d.state = 'suspended' and c.status = 'suspended'
           and (r.subagent_depth = 0 or sr.role is not null)
         for update of s,r,d,c
        """, (row, rowNumber) -> {
          var dispatch = new ExpiredDispatch(
              "suspended", row.getObject("worker_id", UUID.class),
              row.getObject("worker_incarnation_id", UUID.class),
              row.getTimestamp("last_confirmed_healthy_at").toInstant(),
              row.getLong("owner_epoch"), row.getObject("fencing_token", UUID.class),
              row.getObject("session_id", UUID.class), row.getObject("workspace_id", UUID.class),
              row.getObject("agent_version_id", UUID.class),
              row.getObject("model_policy_id", UUID.class), row.getString("input"),
              row.getString("status"), row.getString("placement"), row.getLong("last_sequence"),
              stringList(row, "delegated_scopes"), row.getString("agent_instructions"),
              row.getObject("root_run_id", UUID.class),
              row.getObject("parent_run_id", UUID.class),
              row.getObject("parent_delegation_id", UUID.class), row.getInt("subagent_depth"),
              row.getString("agent_role"), row.getLong("max_tokens"),
              row.getLong("max_cost_cents"), row.getLong("max_duration_seconds"));
          var checkpoint = new StoredCheckpoint(
              row.getObject("checkpoint_id", UUID.class), row.getLong("owner_epoch"),
              row.getObject("fencing_token", UUID.class), row.getLong("last_sequence"),
              "suspended", row.getString("kernel_digest"),
              row.getString("tool_catalog_digest"), row.getBytes("payload"),
              row.getString("payload_ref"), row.getString("payload_encoding"),
              row.getString("payload_digest"), row.getString("stored_payload_digest"),
              row.getLong("uncompressed_size"), row.getLong("stored_size"),
              row.getTimestamp("created_at").toInstant());
          var delivery = new DurableSubagentResult(
              key.toolCallId(), row.getObject("delegation_id", UUID.class),
              row.getString("binding_digest"), row.getObject("child_run_id", UUID.class),
              row.getObject("child_terminal_event_id", UUID.class),
              row.getString("terminal_status"), row.getString("result"),
              row.getBoolean("result_is_error"), row.getString("result_digest"));
          return new SubagentResume(
              row.getObject("parent_attempt_id", UUID.class), dispatch, checkpoint, delivery);
        }, key.tenantId(), key.parentRunId(), key.toolCallId());
    if (results.isEmpty()) {
      return false;
    }
    var resume = results.getFirst();
    var worker = findRecoveryWorker(resume.dispatch(), heartbeatFreshness, true);
    if (worker.isEmpty()) {
      return false;
    }
    var attemptId = dispatchRecovery(
        new DispatchKey(key.tenantId(), key.parentRunId(),
            resume.sourceAttemptId()),
        resume.dispatch(), resume.checkpoint(), worker.orElseThrow(), leaseDuration,
        Optional.empty(), Optional.of(resume.result()));
    var updated = jdbc.update("""
        update subagent_calls set delivery_attempt_id = ?, updated_at = clock_timestamp()
         where tenant_id = ? and parent_run_id = ? and tool_call_id = ?
           and state = 'result_ready' and delivery_attempt_id is null
        """, attemptId, key.tenantId(), key.parentRunId(), key.toolCallId());
    if (updated != 1) {
      throw new IllegalStateException("subagent result dispatch lost its durable binding");
    }
    return true;
  }

  private UUID ensureRecoveryIncident(DispatchKey key, ExpiredDispatch dispatch) {
    var existing = jdbc.query("""
        select incident_id from recovery_incidents
         where tenant_id = ? and run_id = ? and resolved_at is null
         for update
        """, (row, rowNumber) -> row.getObject("incident_id", UUID.class),
        key.tenantId(), key.runId());
    if (!existing.isEmpty()) {
      return existing.getFirst();
    }
    var incidentId = UUID.randomUUID();
    jdbc.update("""
        insert into recovery_incidents (
          tenant_id,incident_id,run_id,failed_attempt_id,failed_worker_id,
          failed_worker_incarnation_id,last_confirmed_healthy_at,state)
        values (?,?,?,?,?,?,?,'waiting_capacity')
        """, key.tenantId(), incidentId, key.runId(), key.attemptId(), dispatch.workerId(),
        dispatch.workerIncarnationId(), Timestamp.from(dispatch.lastConfirmedHealthyAt()));
    return incidentId;
  }

  private void markRecoveryWaitingForCapacity(DispatchKey key, UUID incidentId) {
    jdbc.update("""
        update recovery_incidents
           set state = 'waiting_capacity', updated_at = clock_timestamp()
         where tenant_id = ? and incident_id = ? and resolved_at is null
        """, key.tenantId(), incidentId);
  }

  private void resolveRecoveryIncident(
      DispatchKey key, UUID incidentId, String state, Instant resolvedAt) {
    jdbc.update("""
        update recovery_incidents
           set state = ?, resolved_at = ?, updated_at = clock_timestamp()
         where tenant_id = ? and incident_id = ? and resolved_at is null
        """, state, Timestamp.from(resolvedAt), key.tenantId(), incidentId);
  }

  private Optional<StoredCheckpoint> findSafeCheckpoint(
      DispatchKey key, ExpiredDispatch dispatch) {
    var ambiguous = jdbc.queryForObject("""
        select count(*) from tool_executions
         where tenant_id = ? and run_id = ? and attempt_id = ? and state = 'started'
           and effect in ('non_idempotent', 'unknown')
        """, Integer.class, key.tenantId(), key.runId(), key.attemptId());
    if (ambiguous != null && ambiguous > 0) {
      return Optional.empty();
    }
    return jdbc.query("""
        select checkpoint_id,owner_epoch,fencing_token,sequence,status,
               kernel_digest,tool_catalog_digest,payload,payload_ref,payload_encoding,
               payload_digest,stored_payload_digest,uncompressed_size,stored_size,created_at
          from run_checkpoints
         where tenant_id = ? and run_id = ? and session_id = ? and attempt_id = ?
           and owner_epoch = ? and fencing_token = ? and sequence = ? and status = ?
         order by created_at desc limit 1
        """, (row, rowNumber) -> new StoredCheckpoint(
            row.getObject("checkpoint_id", UUID.class),
            row.getLong("owner_epoch"),
            row.getObject("fencing_token", UUID.class),
            row.getLong("sequence"),
            row.getString("status"),
            row.getString("kernel_digest"),
            row.getString("tool_catalog_digest"),
            row.getBytes("payload"),
            row.getString("payload_ref"),
            row.getString("payload_encoding"),
            row.getString("payload_digest"),
            row.getString("stored_payload_digest"),
            row.getLong("uncompressed_size"),
            row.getLong("stored_size"),
            row.getTimestamp("created_at").toInstant()),
        key.tenantId(), key.runId(), dispatch.sessionId(), key.attemptId(),
        dispatch.ownerEpoch(), dispatch.fencingToken(), dispatch.lastSequence(),
        dispatch.runStatus()).stream().findFirst();
  }

  private Optional<WorkerTarget> findRecoveryWorker(
      ExpiredDispatch dispatch, Duration heartbeatFreshness, boolean allowPrevious) {
    return jdbc.query("""
        select i.worker_id,i.incarnation_id
          from runtime_workers w
          join runtime_worker_incarnations i
            on i.worker_id = w.id and i.incarnation_id = w.current_incarnation_id
         where (? or (i.worker_id,i.incarnation_id) <> (?,?))
           and i.last_heartbeat_received_at >=
             clock_timestamp() - (? * interval '1 millisecond')
           and i.accepting_work
           and i.active_runs < i.capacity
           and (? = 'any' or ? = any(i.placements))
         order by (i.active_runs::numeric / i.capacity),i.last_heartbeat_received_at desc,
                  i.worker_id,i.incarnation_id
         for update skip locked limit 1
        """, (row, rowNumber) -> new WorkerTarget(
            row.getObject("worker_id", UUID.class),
            row.getObject("incarnation_id", UUID.class)),
        allowPrevious, dispatch.workerId(), dispatch.workerIncarnationId(),
        heartbeatFreshness.toMillis(),
        dispatch.placement(), dispatch.placement()).stream().findFirst();
  }

  private UUID dispatchRecovery(
      DispatchKey key,
      ExpiredDispatch previous,
      StoredCheckpoint checkpoint,
      WorkerTarget worker,
      Duration leaseDuration,
      UUID incidentId) {
    return dispatchRecovery(
        key, previous, checkpoint, worker, leaseDuration,
        Optional.of(incidentId), Optional.empty());
  }

  private UUID dispatchRecovery(
      DispatchKey key,
      ExpiredDispatch previous,
      StoredCheckpoint checkpoint,
      WorkerTarget worker,
      Duration leaseDuration,
      Optional<UUID> incidentId,
      Optional<DurableSubagentResult> subagentResult) {
    var steering = findPendingSteering(key);
    var decidedApproval = findUnappliedApprovalDecision(key);
    if (subagentResult.isPresent() && steering.isPresent()) {
      throw new IllegalStateException("recovery cannot deliver a subagent result and steering together");
    }
    var fencingToken = UUID.randomUUID();
    var leases = jdbc.query("""
        insert into workspace_leases (
          tenant_id,workspace_id,owner_id,owner_epoch,fencing_token,expires_at)
        values (?,?,?,1,?,clock_timestamp() + (? * interval '1 millisecond'))
        on conflict (tenant_id,workspace_id) do update
           set owner_id = excluded.owner_id,
               owner_epoch = workspace_leases.owner_epoch + 1,
               fencing_token = excluded.fencing_token,
               expires_at = excluded.expires_at,
               updated_at = clock_timestamp()
         where workspace_leases.expires_at <= clock_timestamp()
        returning owner_epoch,fencing_token,expires_at
        """, (row, rowNumber) -> new LeaseResult(
            row.getLong("owner_epoch"), row.getObject("fencing_token", UUID.class),
            row.getTimestamp("expires_at").toInstant()),
        key.tenantId(), previous.workspaceId(), worker.workerId(), fencingToken,
        leaseDuration.toMillis());
    if (leases.isEmpty()) {
      throw new IllegalStateException("expired workspace lease could not be fenced for recovery");
    }
    var lease = leases.getFirst();
    var issuedAt = Instant.now();
    var attemptId = UUID.randomUUID();
    var executionMessageId = UUID.randomUUID();
    var modelPolicySnapshot = modelPolicySnapshot(key.tenantId(), previous.modelPolicyId());
    var skillSnapshots = skillSnapshots(key.tenantId(), previous.agentVersionId());
    var workloadToken = workloadTokenIssuer.issue(new WorkloadIdentityClaims(
        key.tenantId(), key.runId(), attemptId, worker.workerId(), worker.incarnationId(),
        previous.modelPolicyId(), modelPolicySnapshot.digest(), issuedAt,
        earliest(lease.expiresAt(), issuedAt.plus(Duration.ofMinutes(5)))));
    var command = new RunExecutionCommand(
        EXECUTION_SCHEMA_VERSION,
        executionMessageId,
        key.tenantId(), key.runId(), previous.sessionId(),
        previous.workspaceId(), previous.agentVersionId(), previous.modelPolicyId(), attemptId,
        worker.workerId(), worker.incarnationId(), lease.ownerEpoch(), lease.fencingToken(),
        issuedAt, lease.expiresAt(),
        workloadToken, previous.delegatedScopes(), previous.agentInstructions(),
        modelPolicySnapshot.base64(), modelPolicySnapshot.digest(),
        skillSnapshots,
        previous.lineage(),
        subagentRoles(
            key.tenantId(), previous.agentVersionId(), previous.delegatedScopes(),
            previous.subagentDepth()),
        previous.input(), previous.maxTokens(),
        previous.maxCostCents(), previous.maxDurationSeconds());
    jdbc.update("""
        insert into run_dispatches (
          tenant_id,run_id,attempt_id,worker_id,worker_incarnation_id,owner_epoch,fencing_token,
          lease_expires_at,workload_identity_expires_at,state,requested_at)
        values (?,?,?,?,?,?,?,?,?,'requested',?)
        """, key.tenantId(), key.runId(), attemptId, worker.workerId(), worker.incarnationId(),
        lease.ownerEpoch(),
        lease.fencingToken(), Timestamp.from(lease.expiresAt()), Timestamp.from(lease.expiresAt()),
        Timestamp.from(issuedAt));
    var steeringExpiresAt = issuedAt.plusSeconds(30);
    steering.ifPresent(pending -> {
      var updated = jdbc.update("""
          update run_steering_commands
             set attempt_id = ?, worker_id = ?, worker_incarnation_id = ?,
                 issued_at = ?, expires_at = ?, updated_at = clock_timestamp()
           where tenant_id = ? and run_id = ? and steering_id = ? and attempt_id = ?
             and state = 'pending'
          """, attemptId, worker.workerId(), worker.incarnationId(), Timestamp.from(issuedAt),
          Timestamp.from(steeringExpiresAt), key.tenantId(), key.runId(), pending.steeringId(),
          key.attemptId());
      if (updated != 1) {
        throw new IllegalStateException("pending steering command could not be rebound for recovery");
      }
    });
    incidentId.ifPresent(id -> jdbc.update("""
        update recovery_incidents
           set state = 'recovery_requested', recovery_attempt_id = ?,
               updated_at = clock_timestamp()
         where tenant_id = ? and incident_id = ? and resolved_at is null
        """, attemptId, key.tenantId(), id));
    jdbc.update("""
        update approvals
           set attempt_id = ?, worker_id = ?, worker_incarnation_id = ?
         where tenant_id = ? and run_id = ? and attempt_id = ?
           and status in ('pending','approved','denied')
        """, attemptId, worker.workerId(), worker.incarnationId(), key.tenantId(), key.runId(),
        key.attemptId());
    jdbc.update("""
        update runs set current_attempt_id = ?, updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and current_attempt_id = ?
           and status in ('running','waiting_approval','suspended')
        """, attemptId, key.tenantId(), key.runId(), key.attemptId());
    jdbc.update("""
        update workspaces set state = 'leased', updated_at = clock_timestamp()
         where tenant_id = ? and id = ?
        """, key.tenantId(), previous.workspaceId());
    jdbc.update("""
        update runtime_workers set active_runs = active_runs + 1, updated_at = clock_timestamp()
         where id = ? and current_incarnation_id = ? and accepting_work and active_runs < capacity
        """, worker.workerId(), worker.incarnationId());
    jdbc.update("""
        update runtime_worker_incarnations
           set active_runs = active_runs + 1, updated_at = clock_timestamp()
         where worker_id = ? and incarnation_id = ? and accepting_work and active_runs < capacity
        """, worker.workerId(), worker.incarnationId());
    insertRecoveryOutbox(
        command, key.attemptId(), checkpoint, subagentResult, steering,
        issuedAt, steeringExpiresAt);
    decidedApproval.ifPresent(decision -> insertRecoveredApprovalDecisionOutbox(
        key.tenantId(), key.runId(), attemptId, worker, decision, issuedAt));
    return attemptId;
  }

  private Optional<DurableApprovalDecision> findUnappliedApprovalDecision(DispatchKey key) {
    return jdbc.query("""
        select id,version,status,binding_digest,decision->>'decision' as decision
          from approvals
         where tenant_id = ? and run_id = ? and attempt_id = ?
         order by created_at desc,id desc limit 1
         for update
        """, (row, rowNumber) -> new DurableApprovalDecision(
            row.getObject("id", UUID.class), row.getInt("version"), row.getString("status"),
            row.getString("binding_digest"), row.getString("decision")),
        key.tenantId(), key.runId(), key.attemptId()).stream()
        .filter(approval -> List.of("approved", "denied").contains(approval.status())
            && approval.decision() != null)
        .findFirst();
  }

  private void insertRecoveredApprovalDecisionOutbox(
      UUID tenantId,
      UUID runId,
      UUID attemptId,
      WorkerTarget worker,
      DurableApprovalDecision approval,
      Instant issuedAt) {
    var decision = "allow_session".equals(approval.decision())
        ? "allow_once" : approval.decision();
    if (!List.of("allow_once", "deny").contains(decision)
        || !isSha256(approval.bindingDigest())) {
      throw new IllegalStateException("decided approval has an invalid durable binding");
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
        """, tenantId, messageId, runId, messageId.toString(), tenantId.toString(),
        runId.toString(), attemptId.toString(), worker.workerId().toString(),
        worker.incarnationId().toString(), approval.approvalId().toString(),
        approval.version(), approval.bindingDigest(), decision, issuedAt.toString(),
        issuedAt.plusSeconds(300).toString());
  }

  private Optional<DurableSteering> findPendingSteering(DispatchKey key) {
    return jdbc.query("""
        select steering_id,input,input_digest
          from run_steering_commands
         where tenant_id = ? and run_id = ? and attempt_id = ? and state = 'pending'
         for update
        """, (row, rowNumber) -> new DurableSteering(
            row.getObject("steering_id", UUID.class),
            row.getString("input"),
            row.getString("input_digest")),
        key.tenantId(), key.runId(), key.attemptId()).stream().findFirst();
  }

  private void insertRecoveryOutbox(
      RunExecutionCommand command,
      UUID sourceAttemptId,
      StoredCheckpoint checkpoint,
      Optional<DurableSubagentResult> subagentResult,
      Optional<DurableSteering> steering,
      Instant steeringIssuedAt,
      Instant steeringExpiresAt) {
    var messageId = UUID.randomUUID();
    var payload = JSON.createObjectNode();
    payload.put("schema_version", steering.isPresent() ? 3 : subagentResult.isPresent() ? 2 : 1);
    payload.put("message_id", messageId.toString());
    var execution = payload.putObject("execution");
    execution.put("schema_version", command.schemaVersion());
    execution.put("message_id", command.messageId().toString());
    execution.put("tenant_id", command.tenantId().toString());
    execution.put("run_id", command.runId().toString());
    execution.put("session_id", command.sessionId().toString());
    execution.put("workspace_id", command.workspaceId().toString());
    execution.put("agent_version_id", command.agentVersionId().toString());
    execution.put("model_policy_id", command.modelPolicyId().toString());
    execution.put("attempt_id", command.attemptId().toString());
    execution.put("worker_id", command.workerId().toString());
    execution.put("worker_incarnation_id", command.workerIncarnationId().toString());
    execution.put("owner_epoch", command.ownerEpoch());
    execution.put("fencing_token", command.fencingToken().toString());
    execution.put("issued_at", command.issuedAt().toString());
    execution.put("lease_expires_at", command.leaseExpiresAt().toString());
    execution.put("workload_token", command.workloadToken().value());
    var scopes = execution.putArray("delegated_scopes");
    command.delegatedScopes().forEach(scopes::add);
    execution.put("agent_instructions", command.agentInstructions());
    if (!command.modelPolicySnapshotBase64().isBlank()) {
      execution.put("model_policy_snapshot_base64", command.modelPolicySnapshotBase64());
      execution.put("model_policy_digest", command.modelPolicyDigest());
    }
    execution.set("skill_snapshots", skillSnapshotsNode(command.skillSnapshots()));
    writeLineage(execution, command.lineage());
    execution.set("subagent_roles", subagentRolesNode(command.subagentRoles()));
    execution.put("input", command.input());
    var budget = execution.putObject("budget");
    budget.put("max_tokens", command.maxTokens());
    budget.put("max_cost_cents", command.maxCostCents());
    budget.put("max_duration_seconds", command.maxDurationSeconds());
    var published = payload.putObject("checkpoint");
    published.put("schema_version", checkpoint.payloadEncoding().equals("identity") ? 1 : 2);
    published.put("message_id", checkpoint.checkpointId().toString());
    published.put("tenant_id", command.tenantId().toString());
    published.put("run_id", command.runId().toString());
    published.put("session_id", command.sessionId().toString());
    published.put("attempt_id", sourceAttemptId.toString());
    published.put("owner_epoch", checkpoint.ownerEpoch());
    published.put("fencing_token", checkpoint.fencingToken().toString());
    published.put("sequence", checkpoint.sequence());
    published.put("status", checkpoint.status());
    published.put("kernel_digest", checkpoint.kernelDigest());
    published.put("tool_catalog_digest", checkpoint.toolCatalogDigest());
    if (checkpoint.payload() != null) {
      published.put("payload_base64", Base64.getEncoder().encodeToString(checkpoint.payload()));
    } else {
      published.put("payload_ref", checkpoint.payloadRef());
    }
    published.put("payload_encoding", checkpoint.payloadEncoding());
    published.put("payload_digest", checkpoint.payloadDigest());
    published.put("stored_payload_digest", checkpoint.storedPayloadDigest());
    published.put("uncompressed_size", checkpoint.uncompressedSize());
    published.put("stored_size", checkpoint.storedSize());
    published.put("created_at", checkpoint.createdAt().toString());
    subagentResult.ifPresent(result -> {
      var delivery = payload.putObject("subagent_result");
      delivery.put("tool_call_id", result.toolCallId());
      delivery.put("delegation_id", result.delegationId().toString());
      delivery.put("binding_digest", result.bindingDigest());
      delivery.put("child_run_id", result.childRunId().toString());
      delivery.put("child_terminal_event_id", result.childTerminalEventId().toString());
      delivery.put("terminal_status", result.terminalStatus());
      try {
        delivery.set("content", JSON.readTree(result.content()));
      } catch (JsonProcessingException exception) {
        throw new IllegalStateException("persisted subagent result is malformed", exception);
      }
      delivery.put("is_error", result.isError());
      delivery.put("digest", result.digest());
    });
    steering.ifPresent(pending -> {
      var rebound = payload.putObject("steering");
      rebound.put("schema_version", 1);
      rebound.put("message_id", UUID.randomUUID().toString());
      rebound.put("steering_id", pending.steeringId().toString());
      rebound.put("tenant_id", command.tenantId().toString());
      rebound.put("run_id", command.runId().toString());
      rebound.put("attempt_id", command.attemptId().toString());
      rebound.put("worker_id", command.workerId().toString());
      rebound.put("worker_incarnation_id", command.workerIncarnationId().toString());
      rebound.put("input", pending.input());
      rebound.put("input_digest", pending.inputDigest());
      rebound.put("issued_at", steeringIssuedAt.toString());
      rebound.put("expires_at", steeringExpiresAt.toString());
    });
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        values (?,?,'run',?,'run.recovery.requested',?::jsonb)
        """, command.tenantId(), messageId, command.runId(), payload.toString());
  }

  private int dispatchAttempts(DispatchKey key) {
    var attempts = jdbc.queryForObject("""
        select count(*) from run_dispatches where tenant_id = ? and run_id = ?
        """, Integer.class, key.tenantId(), key.runId());
    return attempts == null ? 0 : attempts;
  }

  private void failNeverAcceptedRun(DispatchKey key, ExpiredDispatch dispatch) {
    var sequence = dispatch.lastSequence() + 1;
    var occurredAt = Instant.now();
    var payload = JSON.createObjectNode();
    payload.put("status", "failed");
    payload.put("reason", "assignment_never_accepted");
    payload.put("dispatch_attempts", dispatchAttempts(key));
    var body = payload.toString();
    jdbc.update("""
        insert into run_events (
          tenant_id,event_id,run_id,session_id,sequence,schema_version,attempt_id,
          occurred_at,trace_id,type,payload,digest)
        values (?,?,?,?,?,1,?,?,?,'run.failed',?::jsonb,?)
        """, key.tenantId(), UUID.randomUUID(), key.runId(), dispatch.sessionId(), sequence,
        key.attemptId(), Timestamp.from(occurredAt), UUID.randomUUID().toString(), body,
        sha256(body));
    jdbc.update("""
        update runs
           set status = 'failed', current_attempt_id = null, last_sequence = ?,
               finished_at = ?, updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and current_attempt_id = ? and status = 'queued'
        """, sequence, Timestamp.from(occurredAt), key.tenantId(), key.runId(), key.attemptId());
    // Release only this dispatch's lease generation so a workspace another Run
    // has legitimately taken over is never stolen.
    jdbc.update("""
        update workspace_leases
           set expires_at = clock_timestamp(), updated_at = clock_timestamp()
         where tenant_id = ? and workspace_id = ? and owner_id = ?
           and owner_epoch = ? and fencing_token = ?
        """, key.tenantId(), dispatch.workspaceId(), dispatch.workerId(), dispatch.ownerEpoch(),
        dispatch.fencingToken());
    jdbc.update("""
        update workspaces set state = 'ready', updated_at = clock_timestamp()
         where tenant_id = ? and id = ?
           and not exists (
             select 1 from workspace_leases l
              where l.tenant_id = workspaces.tenant_id and l.workspace_id = workspaces.id
                and l.expires_at > clock_timestamp())
        """, key.tenantId(), dispatch.workspaceId());
    rejectPendingSteering(key.tenantId(), key.runId(), "assignment_never_accepted");
  }

  private String indeterminatePayload(DispatchKey key) {
    var ambiguous = jdbc.query("""
        select tool_call_id,binding_digest,effect
          from tool_executions
         where tenant_id = ? and run_id = ? and attempt_id = ? and state = 'started'
         order by started_at,tool_call_id
         limit 1
        """, (row, rowNumber) -> new AmbiguousToolExecution(
            row.getString("tool_call_id"),
            row.getString("binding_digest"),
            row.getString("effect")), key.tenantId(), key.runId(), key.attemptId());
    var payload = JSON.createObjectNode();
    payload.put("status", "indeterminate");
    payload.put("replay_safe", false);
    if (!ambiguous.isEmpty()
        && List.of("non_idempotent", "unknown").contains(ambiguous.getFirst().effect())) {
      var tool = ambiguous.getFirst();
      payload.put("reason", "ambiguous_non_idempotent_tool");
      payload.put("tool_call_id", tool.toolCallId());
      payload.put("binding_digest", tool.bindingDigest());
      payload.put("effect", tool.effect());
    } else if (!ambiguous.isEmpty()) {
      var tool = ambiguous.getFirst();
      payload.put("reason", "checkpoint_missing_after_replay_safe_tool_start");
      payload.put("tool_call_id", tool.toolCallId());
      payload.put("binding_digest", tool.bindingDigest());
      payload.put("effect", tool.effect());
    } else {
      payload.put("reason", "worker_lost_without_checkpoint");
    }
    try {
      return JSON.writeValueAsString(payload);
    } catch (JsonProcessingException exception) {
      throw new IllegalStateException("failed to encode indeterminate recovery evidence", exception);
    }
  }

  private void insertRunQueuedOutbox(UUID tenantId, UUID runId) {
    var outboxId = UUID.randomUUID();
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        select r.tenant_id,?,'run',r.id,'run.queued',jsonb_build_object(
          'schema_version',1,
          'message_id',?::text,
          'tenant_id',r.tenant_id::text,
          'run_id',r.id::text,
          'session_id',r.session_id::text,
          'workspace_id',r.workspace_id::text,
          'agent_version_id',r.agent_version_id::text,
          'model_policy_id',r.model_policy_id::text,
          'occurred_at',clock_timestamp()::text,
          'input',r.input,
          'priority',case r.priority when 0 then 'interactive' else 'batch' end,
          'placement',r.placement,
          'budget',jsonb_build_object(
            'max_tokens',r.max_tokens,
            'max_cost_cents',r.max_cost_cents,
            'max_duration_seconds',r.max_duration_seconds))
          from runs r where r.tenant_id = ? and r.id = ? and r.status = 'queued'
        """, outboxId, outboxId.toString(), tenantId, runId);
  }

  private String sha256(String value) {
    return sha256(value.getBytes(StandardCharsets.UTF_8));
  }

  private String sha256(byte[] value) {
    try {
      return HexFormat.of().formatHex(
          MessageDigest.getInstance("SHA-256").digest(value));
    } catch (NoSuchAlgorithmException impossible) {
      throw new IllegalStateException("SHA-256 is unavailable", impossible);
    }
  }

  public boolean recordAcceptance(ExecutionAcceptedMessage accepted) {
    return Boolean.TRUE.equals(transactions.execute(status -> {
      setTenant(accepted.tenantId());
      var updated = jdbc.update("""
          update run_dispatches
             set state = 'accepted', accepted_at = ?, updated_at = clock_timestamp()
           where tenant_id = ? and run_id = ? and attempt_id = ? and worker_id = ?
             and worker_incarnation_id = ?
             and state = 'requested'
          """, Timestamp.from(accepted.acceptedAt()), accepted.tenantId(), accepted.runId(),
          accepted.attemptId(), accepted.workerId(), accepted.workerIncarnationId());
      if (updated == 1) {
        jdbc.update("""
            update runs
               set status = 'running', updated_at = clock_timestamp()
             where tenant_id = ? and id = ? and current_attempt_id = ? and status = 'queued'
            """, accepted.tenantId(), accepted.runId(), accepted.attemptId());
        return true;
      }
      return jdbc.queryForObject("""
          select count(*)
            from run_dispatches
           where tenant_id = ? and run_id = ? and attempt_id = ? and worker_id = ?
             and worker_incarnation_id = ?
             and state = 'accepted'
          """, Integer.class, accepted.tenantId(), accepted.runId(), accepted.attemptId(),
          accepted.workerId(), accepted.workerIncarnationId()) == 1;
    }));
  }

  public boolean recordRunEvent(RunEventMessage event) {
    if (!sha256(event.payload()).equals(event.digest())) {
      return false;
    }
    return Boolean.TRUE.equals(transactions.execute(status -> {
      setTenant(event.tenantId());
      var existing = jdbc.queryForObject("""
          select count(*) from run_events where tenant_id = ? and event_id = ?
          """, Integer.class, event.tenantId(), event.eventId());
      if (existing != null && existing == 1) {
        var exact = jdbc.queryForObject("""
            select count(*) from run_events
             where tenant_id = ? and event_id = ? and run_id = ? and session_id = ?
               and sequence = ? and attempt_id = ? and type = ? and digest = ?
            """, Integer.class, event.tenantId(), event.eventId(), event.runId(),
            event.sessionId(), event.sequence(), event.attemptId(), event.type(), event.digest());
        return exact != null && exact == 1;
      }
      var runs = jdbc.query("""
          select runs.last_sequence,runs.status,runs.application_id,runs.session_id,
                 runs.workspace_id,runs.agent_version_id,d.worker_id,
                 d.worker_incarnation_id,d.owner_epoch,d.fencing_token
            from runs
            join run_dispatches d
              on d.tenant_id = runs.tenant_id and d.run_id = runs.id
             and d.attempt_id = runs.current_attempt_id
             and d.state in ('accepted','suspended')
           where runs.tenant_id = ? and runs.id = ? and runs.session_id = ?
             and runs.current_attempt_id = ?
             and runs.status in ('running','waiting_approval','suspended')
           for update of runs,d
          """, (row, rowNumber) -> new ActiveRun(
              row.getLong("last_sequence"), row.getString("status"),
              row.getObject("application_id", UUID.class),
              row.getObject("session_id", UUID.class),
              row.getObject("workspace_id", UUID.class),
              row.getObject("agent_version_id", UUID.class),
              row.getObject("worker_id", UUID.class),
              row.getObject("worker_incarnation_id", UUID.class), row.getLong("owner_epoch"),
              row.getObject("fencing_token", UUID.class)), event.tenantId(), event.runId(),
          event.sessionId(), event.attemptId());
      if (runs.isEmpty() || event.sequence() != runs.getFirst().lastSequence() + 1
          || !acceptsEvent(runs.getFirst().status(), event)) {
        return false;
      }
      var toolMutation = parseToolLedgerMutation(event);
      if (!acceptsToolLedgerMutation(event, toolMutation)) {
        return false;
      }
      var steeringReceipt = parseSteeringReceipt(event);
      if ("run.steer.applied".equals(event.type())
          && (steeringReceipt.isEmpty()
              || !acceptsSteeringReceipt(event, steeringReceipt.get()))) {
        return false;
      }
      jdbc.update("""
          insert into run_events (
            tenant_id,event_id,run_id,session_id,sequence,schema_version,attempt_id,
            occurred_at,trace_id,type,payload,digest)
          values (?,?,?,?,?,?,?,?,?,?,?::jsonb,?)
          """, event.tenantId(), event.eventId(), event.runId(), event.sessionId(),
          event.sequence(), event.schemaVersion(), event.attemptId(),
          Timestamp.from(event.timestamp()), event.traceId().toString(), event.type(),
          event.payload(), event.digest());
      applyToolLedgerMutation(event, toolMutation);
      steeringReceipt.ifPresent(receipt -> applySteeringReceipt(event, receipt));
      if ("subagent.spawn.requested".equals(event.type())) {
        persistSubagentRequest(event);
      }
      if ("run.restored".equals(event.type())) {
        jdbc.update("""
            update recovery_incidents
               set state = 'recovered', resolved_at = clock_timestamp(),
                   updated_at = clock_timestamp()
             where tenant_id = ? and run_id = ? and recovery_attempt_id = ?
               and resolved_at is null
            """, event.tenantId(), event.runId(), event.attemptId());
      } else if (event.isTerminal()) {
        var incidentState = "run.indeterminate".equals(event.type())
            ? "indeterminate" : "terminated";
        jdbc.update("""
            update recovery_incidents
               set state = ?, resolved_at = clock_timestamp(), updated_at = clock_timestamp()
             where tenant_id = ? and run_id = ? and recovery_attempt_id = ?
               and resolved_at is null
            """, incidentState, event.tenantId(), event.runId(), event.attemptId());
      }
      if (event.isTerminal()) {
        finishRun(event, runs.getFirst());
        completeSubagentResult(event);
      } else if ("approval.required".equals(event.type())) {
        persistApproval(event, runs.getFirst());
      } else if ("run.resumed".equals(event.type())) {
        jdbc.update("""
            update runs set status = 'running', last_sequence = ?, updated_at = clock_timestamp()
             where tenant_id = ? and id = ? and current_attempt_id = ?
               and status = 'waiting_approval'
            """, event.sequence(), event.tenantId(), event.runId(), event.attemptId());
      } else if ("subagent.result.received".equals(event.type())) {
        deliverSubagentResult(event);
      } else {
        jdbc.update("""
            update runs set last_sequence = ?, updated_at = clock_timestamp()
             where tenant_id = ? and id = ? and current_attempt_id = ?
            """, event.sequence(), event.tenantId(), event.runId(), event.attemptId());
      }
      return true;
    }));
  }

  public boolean recordSteeringOutcome(RunSteeringOutcomeMessage outcome) {
    return Boolean.TRUE.equals(transactions.execute(status -> {
      setTenant(outcome.tenantId());
      var commands = jdbc.query("""
          select state,outcome_message_id,rejection_reason
            from run_steering_commands
           where tenant_id = ? and run_id = ? and steering_id = ? and attempt_id = ?
             and worker_id = ? and worker_incarnation_id = ? and input_digest = ?
           for update
          """, (row, rowNumber) -> new SteeringOutcomeState(
              row.getString("state"), row.getObject("outcome_message_id", UUID.class),
              row.getString("rejection_reason")),
          outcome.tenantId(), outcome.runId(), outcome.steeringId(), outcome.attemptId(),
          outcome.workerId(), outcome.workerIncarnationId(), outcome.inputDigest());
      if (commands.isEmpty()) {
        return false;
      }
      var command = commands.getFirst();
      if ("rejected".equals(command.state())) {
        return outcome.messageId().equals(command.outcomeMessageId())
            && outcome.reason().equals(command.rejectionReason());
      }
      if (!"pending".equals(command.state())) {
        return false;
      }
      return jdbc.update("""
          update run_steering_commands
             set state = 'rejected', outcome_message_id = ?, rejection_reason = ?,
                 rejected_at = ?, updated_at = clock_timestamp()
           where tenant_id = ? and run_id = ? and steering_id = ? and attempt_id = ?
             and worker_id = ? and worker_incarnation_id = ? and input_digest = ?
             and state = 'pending'
          """, outcome.messageId(), outcome.reason(), Timestamp.from(outcome.occurredAt()),
          outcome.tenantId(), outcome.runId(), outcome.steeringId(), outcome.attemptId(),
          outcome.workerId(), outcome.workerIncarnationId(), outcome.inputDigest()) == 1;
    }));
  }

  public Optional<ToolExecutionReceipt> findToolExecution(
      UUID tenantId, UUID runId, UUID attemptId, String toolCallId) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      return jdbc.query("""
          select tenant_id,run_id,attempt_id,tool_call_id,binding_digest,effect,sandbox,state,
                 requested_event_id,started_event_id,result_event_id
            from tool_executions
           where tenant_id = ? and run_id = ? and attempt_id = ? and tool_call_id = ?
          """, (row, rowNumber) -> new ToolExecutionReceipt(
              row.getObject("tenant_id", UUID.class),
              row.getObject("run_id", UUID.class),
              row.getObject("attempt_id", UUID.class),
              row.getString("tool_call_id"),
              row.getString("binding_digest"),
              row.getString("effect"),
              row.getString("sandbox"),
              row.getString("state"),
              row.getObject("requested_event_id", UUID.class),
              row.getObject("started_event_id", UUID.class),
              row.getObject("result_event_id", UUID.class)),
          tenantId, runId, attemptId, toolCallId).stream().findFirst();
    });
  }

  public RecoverySloSnapshot recoverySloSnapshot(UUID tenantId, Duration objective) {
    validateDuration(objective, "recovery objective", Duration.ofHours(24));
    return transactions.execute(status -> {
      setTenant(tenantId);
      return jdbc.queryForObject("""
          select count(*)::integer as open_incidents,
                 count(*) filter (
                   where last_confirmed_healthy_at <=
                     clock_timestamp() - (? * interval '1 millisecond'))::integer
                   as overdue_incidents,
                 count(*) filter (where state = 'waiting_capacity')::integer
                   as waiting_capacity,
                 count(*) filter (where state = 'recovery_requested')::integer
                   as recovery_requested,
                 coalesce(extract(epoch from (
                   clock_timestamp() - min(last_confirmed_healthy_at))) * 1000, 0)::bigint
                   as oldest_open_age_millis
            from recovery_incidents
           where tenant_id = ? and resolved_at is null
          """, (row, rowNumber) -> new RecoverySloSnapshot(
              row.getInt("open_incidents"), row.getInt("overdue_incidents"),
              row.getInt("waiting_capacity"), row.getInt("recovery_requested"),
              row.getLong("oldest_open_age_millis")),
          objective.toMillis(), tenantId);
    });
  }

  @Override
  public RecoverySloSnapshot globalRecoverySloSnapshot(Duration objective) {
    validateDuration(objective, "recovery objective", Duration.ofHours(24));
    return jdbc.queryForObject("""
        select coalesce(sum(waiting_capacity + recovery_requested),0)::bigint
                 as open_incidents,
               coalesce(sum(case
                 when last_confirmed_healthy_at <=
                   clock_timestamp() - (? * interval '1 millisecond')
                 then waiting_capacity + recovery_requested else 0 end),0)::bigint
                 as overdue_incidents,
               coalesce(sum(waiting_capacity),0)::bigint as waiting_capacity,
               coalesce(sum(recovery_requested),0)::bigint as recovery_requested,
               coalesce(extract(epoch from (
                 clock_timestamp() - min(last_confirmed_healthy_at))) * 1000,0)::bigint
                 as oldest_open_age_millis
          from recovery_metric_buckets
        """, (row, rowNumber) -> new RecoverySloSnapshot(
            Math.toIntExact(row.getLong("open_incidents")),
            Math.toIntExact(row.getLong("overdue_incidents")),
            Math.toIntExact(row.getLong("waiting_capacity")),
            Math.toIntExact(row.getLong("recovery_requested")),
            row.getLong("oldest_open_age_millis")),
        objective.toMillis());
  }

  public boolean recordCheckpoint(RunCheckpointMessage checkpoint) {
    if ((checkpoint.payload() != null
            && !sha256(checkpoint.payload()).equals(checkpoint.storedPayloadDigest()))
        || !isSha256(checkpoint.payloadDigest())
        || !isSha256(checkpoint.storedPayloadDigest())
        || !isSha256(checkpoint.kernelDigest())
        || !isSha256(checkpoint.toolCatalogDigest())
        || !List.of("running", "waiting_approval", "suspended").contains(checkpoint.status())) {
      return false;
    }
    return Boolean.TRUE.equals(transactions.execute(status -> {
      setTenant(checkpoint.tenantId());
      if (isExactCheckpoint(checkpoint)) {
        return true;
      }
      var active = jdbc.queryForObject("""
          select count(*)
            from runs r
            join run_dispatches d
              on d.tenant_id = r.tenant_id and d.run_id = r.id
             and d.attempt_id = r.current_attempt_id
           where r.tenant_id = ? and r.id = ? and r.session_id = ?
             and r.current_attempt_id = ? and r.last_sequence = ?
             and ((? = 'suspended' and r.status = 'running' and exists (
                    select 1 from subagent_calls s
                     where s.tenant_id = r.tenant_id and s.parent_run_id = r.id
                       and s.parent_attempt_id = r.current_attempt_id
                       and s.request_sequence = ? and s.state = 'awaiting_checkpoint'))
                  or r.status = ?)
             and d.state = 'accepted' and d.owner_epoch = ? and d.fencing_token = ?
          """, Integer.class, checkpoint.tenantId(), checkpoint.runId(), checkpoint.sessionId(),
          checkpoint.attemptId(), checkpoint.sequence(), checkpoint.status(), checkpoint.sequence(),
          checkpoint.status(),
          checkpoint.ownerEpoch(), checkpoint.fencingToken());
      if (active == null || active != 1) {
        return false;
      }
      jdbc.update("""
          insert into run_checkpoints (
            tenant_id,checkpoint_id,run_id,session_id,attempt_id,owner_epoch,fencing_token,
            sequence,status,schema_version,kernel_digest,tool_catalog_digest,payload,
            payload_ref,payload_encoding,payload_digest,stored_payload_digest,
            uncompressed_size,stored_size,created_at)
          values (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
          on conflict do nothing
          """, checkpoint.tenantId(), checkpoint.checkpointId(), checkpoint.runId(),
          checkpoint.sessionId(), checkpoint.attemptId(), checkpoint.ownerEpoch(),
          checkpoint.fencingToken(), checkpoint.sequence(), checkpoint.status(),
          checkpoint.schemaVersion(), checkpoint.kernelDigest(), checkpoint.toolCatalogDigest(),
          checkpoint.payload(), checkpoint.payloadRef(), checkpoint.payloadEncoding(),
          checkpoint.payloadDigest(), checkpoint.storedPayloadDigest(),
          checkpoint.uncompressedSize(), checkpoint.storedSize(),
          Timestamp.from(checkpoint.createdAt()));
      if (!isExactCheckpoint(checkpoint)) {
        return false;
      }
      if ("suspended".equals(checkpoint.status())) {
        handoffSubagent(checkpoint);
      }
      return true;
    }));
  }

  private boolean isExactCheckpoint(RunCheckpointMessage checkpoint) {
    var exact = jdbc.queryForObject("""
        select count(*) from run_checkpoints
         where tenant_id = ? and checkpoint_id = ? and run_id = ? and session_id = ?
           and attempt_id = ? and owner_epoch = ? and fencing_token = ? and sequence = ?
           and status = ? and schema_version = ? and kernel_digest = ?
           and tool_catalog_digest = ? and payload_digest = ? and stored_payload_digest = ?
           and payload is not distinct from ? and payload_ref is not distinct from ?
           and payload_encoding = ? and uncompressed_size = ? and stored_size = ?
        """, Integer.class, checkpoint.tenantId(), checkpoint.checkpointId(),
        checkpoint.runId(), checkpoint.sessionId(), checkpoint.attemptId(),
        checkpoint.ownerEpoch(), checkpoint.fencingToken(), checkpoint.sequence(),
        checkpoint.status(), checkpoint.schemaVersion(), checkpoint.kernelDigest(),
        checkpoint.toolCatalogDigest(), checkpoint.payloadDigest(),
        checkpoint.storedPayloadDigest(), checkpoint.payload(), checkpoint.payloadRef(),
        checkpoint.payloadEncoding(), checkpoint.uncompressedSize(), checkpoint.storedSize());
    return exact != null && exact == 1;
  }

  private void handoffSubagent(RunCheckpointMessage checkpoint) {
    var pending = jdbc.query("""
        select r.application_id,s.tool_call_id,s.delegation_id,s.role,s.input,
               s.max_tokens,s.max_cost_cents,s.max_duration_seconds,s.binding_digest
          from subagent_calls s
          join runs r on r.tenant_id = s.tenant_id and r.id = s.parent_run_id
         where s.tenant_id = ? and s.parent_run_id = ? and s.parent_attempt_id = ?
           and s.request_sequence = ? and s.state = 'awaiting_checkpoint'
         for update of s,r
        """, (row, rowNumber) -> new DurableSubagentHandoff(
            row.getObject("application_id", UUID.class),
            row.getString("tool_call_id"),
            row.getString("binding_digest"),
            new SpawnSubagentCommand(
                row.getObject("delegation_id", UUID.class), row.getString("role"),
                row.getString("input"), row.getLong("max_tokens"),
                row.getLong("max_cost_cents"), row.getLong("max_duration_seconds"))),
        checkpoint.tenantId(), checkpoint.runId(), checkpoint.attemptId(),
        checkpoint.sequence());
    if (pending.size() != 1) {
      throw new IllegalStateException("suspended checkpoint has no unique subagent request");
    }
    var request = pending.getFirst();
    var admission = subagentAdmission.admit(
        checkpoint.tenantId(), request.applicationId(), checkpoint.runId(), request.command());
    var updated = jdbc.update("""
        update subagent_calls
           set state = 'child_queued', parent_checkpoint_id = ?, child_run_id = ?,
               updated_at = clock_timestamp()
         where tenant_id = ? and parent_run_id = ? and parent_attempt_id = ?
           and tool_call_id = ? and binding_digest = ? and state = 'awaiting_checkpoint'
        """, checkpoint.checkpointId(), admission.childRunId(), checkpoint.tenantId(),
        checkpoint.runId(), checkpoint.attemptId(), request.toolCallId(),
        request.bindingDigest());
    if (updated != 1) {
      throw new IllegalStateException("subagent handoff lost its durable request");
    }
    updated = jdbc.update("""
        update runs set status = 'suspended', updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and current_attempt_id = ? and status = 'running'
        """, checkpoint.tenantId(), checkpoint.runId(), checkpoint.attemptId());
    if (updated != 1) {
      throw new IllegalStateException("subagent handoff lost parent run ownership");
    }
    jdbc.update("""
        update run_dispatches set state = 'suspended', updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and attempt_id = ? and state = 'accepted'
        """, checkpoint.tenantId(), checkpoint.runId(), checkpoint.attemptId());
    jdbc.update("""
        update runtime_workers
           set active_runs = greatest(active_runs - 1, 0), updated_at = clock_timestamp()
         where id = (select worker_id from run_dispatches
                       where tenant_id = ? and run_id = ? and attempt_id = ?)
        """, checkpoint.tenantId(), checkpoint.runId(), checkpoint.attemptId());
    jdbc.update("""
        update runtime_worker_incarnations i
           set active_runs = greatest(i.active_runs - 1, 0), updated_at = clock_timestamp()
          from run_dispatches d
         where d.tenant_id = ? and d.run_id = ? and d.attempt_id = ?
           and i.worker_id = d.worker_id and i.incarnation_id = d.worker_incarnation_id
        """, checkpoint.tenantId(), checkpoint.runId(), checkpoint.attemptId());
    jdbc.update("""
        update workspace_leases
           set expires_at = clock_timestamp(), updated_at = clock_timestamp()
         where tenant_id = ? and workspace_id = (
           select workspace_id from runs where tenant_id = ? and id = ?)
           and owner_epoch = ? and fencing_token = ?
        """, checkpoint.tenantId(), checkpoint.tenantId(), checkpoint.runId(),
        checkpoint.ownerEpoch(), checkpoint.fencingToken());
    jdbc.update("""
        update workspaces set state = 'ready', updated_at = clock_timestamp()
         where tenant_id = ? and id = (
           select workspace_id from runs where tenant_id = ? and id = ?)
        """, checkpoint.tenantId(), checkpoint.tenantId(), checkpoint.runId());
  }

  public RecoveryEligibility assessRecovery(UUID tenantId, UUID runId, UUID attemptId) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      var checkpoints = jdbc.query("""
          select c.sequence,c.status,d.owner_epoch = c.owner_epoch as same_epoch,
                 d.fencing_token = c.fencing_token as same_fence,r.last_sequence,r.status run_status
            from run_checkpoints c
            join run_dispatches d
              on d.tenant_id = c.tenant_id and d.run_id = c.run_id
             and d.attempt_id = c.attempt_id
            join runs r on r.tenant_id = c.tenant_id and r.id = c.run_id
           where c.tenant_id = ? and c.run_id = ? and c.attempt_id = ?
           order by c.sequence desc limit 1
          """, (row, rowNumber) -> new CheckpointRecoveryState(
              row.getLong("sequence"), row.getString("status"),
              row.getBoolean("same_epoch"), row.getBoolean("same_fence"),
              row.getLong("last_sequence"), row.getString("run_status")),
          tenantId, runId, attemptId);
      if (checkpoints.isEmpty()) {
        return RecoveryEligibility.MISSING;
      }
      var checkpoint = checkpoints.getFirst();
      if (checkpoint.sequence() != checkpoint.lastSequence()
          || !checkpoint.status().equals(checkpoint.runStatus())
          || !checkpoint.sameEpoch() || !checkpoint.sameFence()) {
        return RecoveryEligibility.STALE;
      }
      var ambiguous = jdbc.queryForObject("""
          select count(*) from tool_executions
           where tenant_id = ? and run_id = ? and attempt_id = ? and state = 'started'
             and effect in ('non_idempotent', 'unknown')
          """, Integer.class, tenantId, runId, attemptId);
      return ambiguous != null && ambiguous > 0
          ? RecoveryEligibility.AMBIGUOUS_SIDE_EFFECT
          : RecoveryEligibility.SAFE;
    });
  }

  private boolean isSha256(String value) {
    return value != null && value.matches("^[0-9a-f]{64}$");
  }

  private ToolLedgerMutation parseToolLedgerMutation(RunEventMessage event) {
    try {
      var root = JSON.readTree(event.payload());
      if ("tool.execution.requested".equals(event.type())) {
        return plannedToolMutation(root.path("execution"));
      }
      if ("approval.required".equals(event.type()) || "approval.rebound".equals(event.type())) {
        return plannedToolMutation(root.path("approval").path("execution"));
      }
      if ("tool.execution.started".equals(event.type())) {
        return startedToolMutation(root.path("execution"));
      }
      if ("tool.result".equals(event.type())) {
        var toolCallId = root.path("tool_call_id").asText();
        var bindingDigest = root.path("binding_digest").asText();
        validateToolBinding(toolCallId, bindingDigest);
        var errorCode = root.path("content").path("error").path("code").asText();
        var denial = root.path("is_error").asBoolean(false)
            && ("approval_denied".equals(errorCode) || "tool_policy_denied".equals(errorCode));
        return new ToolLedgerMutation(
            ToolLedgerMutationType.COMPLETE, toolCallId, bindingDigest,
            null, null, null, denial);
      }
      return ToolLedgerMutation.none();
    } catch (JsonProcessingException | IllegalArgumentException exception) {
      throw new IllegalArgumentException("tool execution event payload is malformed", exception);
    }
  }

  private ToolLedgerMutation plannedToolMutation(com.fasterxml.jackson.databind.JsonNode execution) {
    var toolCallId = execution.path("call").path("id").asText();
    var bindingDigest = execution.path("binding_digest").asText();
    var effect = execution.path("effect").asText();
    var sandbox = execution.path("sandbox").asText();
    validateToolBinding(toolCallId, bindingDigest);
    if (!List.of("pure", "idempotent", "non_idempotent", "unknown").contains(effect)
        || !List.of("restricted_container", "kata", "trusted_native").contains(sandbox)) {
      throw new IllegalArgumentException("tool execution policy is invalid");
    }
    return new ToolLedgerMutation(
        ToolLedgerMutationType.PLAN, toolCallId, bindingDigest,
        effect, sandbox, execution.toString(), false);
  }

  private ToolLedgerMutation startedToolMutation(com.fasterxml.jackson.databind.JsonNode execution) {
    var planned = plannedToolMutation(execution);
    return new ToolLedgerMutation(
        ToolLedgerMutationType.START, planned.toolCallId(), planned.bindingDigest(),
        planned.effect(), planned.sandbox(), planned.request(), false);
  }

  private void validateToolBinding(String toolCallId, String bindingDigest) {
    if (toolCallId == null || toolCallId.isBlank() || toolCallId.length() > 256
        || bindingDigest == null || !bindingDigest.matches("[0-9a-f]{64}")) {
      throw new IllegalArgumentException("tool execution binding is invalid");
    }
  }

  private boolean acceptsToolLedgerMutation(
      RunEventMessage event, ToolLedgerMutation mutation) {
    if (mutation.type() == ToolLedgerMutationType.NONE) {
      return true;
    }
    var existing = findToolExecutionState(event, mutation);
    return switch (mutation.type()) {
      case NONE -> true;
      case PLAN -> existing.isEmpty();
      case START -> existing.stream().anyMatch(receipt ->
          "planned".equals(receipt.state())
              && receipt.bindingDigest().equals(mutation.bindingDigest()));
      case COMPLETE -> existing.isEmpty() || existing.stream().anyMatch(receipt ->
          receipt.bindingDigest().equals(mutation.bindingDigest())
              && ("started".equals(receipt.state())
                  || (mutation.denial() && "planned".equals(receipt.state()))));
    };
  }

  private List<ToolExecutionReceipt> findToolExecutionState(
      RunEventMessage event, ToolLedgerMutation mutation) {
    return jdbc.query("""
        select tenant_id,run_id,attempt_id,tool_call_id,binding_digest,effect,sandbox,state,
               requested_event_id,started_event_id,result_event_id
          from tool_executions
         where tenant_id = ? and run_id = ? and attempt_id = ? and tool_call_id = ?
         for update
        """, (row, rowNumber) -> new ToolExecutionReceipt(
            row.getObject("tenant_id", UUID.class), row.getObject("run_id", UUID.class),
            row.getObject("attempt_id", UUID.class), row.getString("tool_call_id"),
            row.getString("binding_digest"), row.getString("effect"),
            row.getString("sandbox"), row.getString("state"),
            row.getObject("requested_event_id", UUID.class),
            row.getObject("started_event_id", UUID.class),
            row.getObject("result_event_id", UUID.class)),
        event.tenantId(), event.runId(), event.attemptId(), mutation.toolCallId());
  }

  private void applyToolLedgerMutation(RunEventMessage event, ToolLedgerMutation mutation) {
    switch (mutation.type()) {
      case NONE -> { }
      case PLAN -> jdbc.update("""
          insert into tool_executions (
            tenant_id,run_id,attempt_id,tool_call_id,binding_digest,effect,sandbox,state,
            request,requested_event_id,requested_at)
          values (?,?,?,?,?,?,?,'planned',?::jsonb,?,?)
          """, event.tenantId(), event.runId(), event.attemptId(), mutation.toolCallId(),
          mutation.bindingDigest(), mutation.effect(), mutation.sandbox(), mutation.request(),
          event.eventId(), Timestamp.from(event.timestamp()));
      case START -> jdbc.update("""
          update tool_executions
             set state = 'started', started_event_id = ?, started_at = ?, updated_at = now()
           where tenant_id = ? and run_id = ? and attempt_id = ? and tool_call_id = ?
             and binding_digest = ? and state = 'planned'
          """, event.eventId(), Timestamp.from(event.timestamp()), event.tenantId(), event.runId(),
          event.attemptId(), mutation.toolCallId(), mutation.bindingDigest());
      case COMPLETE -> jdbc.update("""
          update tool_executions
             set state = 'completed', result_event_id = ?, completed_at = ?, updated_at = now()
           where tenant_id = ? and run_id = ? and attempt_id = ? and tool_call_id = ?
             and binding_digest = ? and state in ('planned','started')
          """, event.eventId(), Timestamp.from(event.timestamp()), event.tenantId(), event.runId(),
          event.attemptId(), mutation.toolCallId(), mutation.bindingDigest());
    }
  }

  private boolean acceptsEvent(String status, RunEventMessage event) {
    if ("subagent.spawn.requested".equals(event.type())) {
      return "running".equals(status);
    }
    if ("approval.required".equals(event.type())) {
      return "running".equals(status);
    }
    if ("approval.rebound".equals(event.type())) {
      return "waiting_approval".equals(status);
    }
    if ("run.restored".equals(event.type())) {
      return List.of("running", "waiting_approval", "suspended").contains(status);
    }
    if ("subagent.result.received".equals(event.type())) {
      return "suspended".equals(status);
    }
    if ("run.resumed".equals(event.type())) {
      return "waiting_approval".equals(status);
    }
    return "running".equals(status)
        || (List.of("waiting_approval", "suspended").contains(status) && event.isTerminal());
  }

  private Optional<SteeringReceipt> parseSteeringReceipt(RunEventMessage event) {
    if (!"run.steer.applied".equals(event.type())) {
      return Optional.empty();
    }
    try {
      var payload = JSON.readTree(event.payload());
      var steeringId = UUID.fromString(payload.path("steering_id").asText());
      var inputDigest = payload.path("input_digest").asText();
      if (!inputDigest.matches("^[0-9a-f]{64}$")) {
        return Optional.empty();
      }
      return Optional.of(new SteeringReceipt(steeringId, inputDigest));
    } catch (JsonProcessingException | IllegalArgumentException invalidPayload) {
      return Optional.empty();
    }
  }

  private boolean acceptsSteeringReceipt(RunEventMessage event, SteeringReceipt receipt) {
    var matching = jdbc.queryForObject("""
        select count(*) from run_steering_commands
         where tenant_id = ? and run_id = ? and attempt_id = ? and steering_id = ?
           and input_digest = ? and state = 'pending'
        """, Integer.class, event.tenantId(), event.runId(), event.attemptId(),
        receipt.steeringId(), receipt.inputDigest());
    return matching != null && matching == 1;
  }

  private void applySteeringReceipt(RunEventMessage event, SteeringReceipt receipt) {
    var updated = jdbc.update("""
        update run_steering_commands
           set state = 'applied', applied_event_id = ?, updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and attempt_id = ? and steering_id = ?
           and input_digest = ? and state = 'pending'
        """, event.eventId(), event.tenantId(), event.runId(), event.attemptId(),
        receipt.steeringId(), receipt.inputDigest());
    if (updated != 1) {
      throw new IllegalStateException("run steering receipt lost its pending command");
    }
  }

  private void persistSubagentRequest(RunEventMessage event) {
    var request = parseSubagentRequest(event.payload());
    var inserted = jdbc.update("""
        insert into subagent_calls (
          tenant_id,parent_run_id,parent_attempt_id,tool_call_id,delegation_id,role,input,
          max_tokens,max_cost_cents,max_duration_seconds,binding_digest,request_event_id,
          request_sequence)
        values (?,?,?,?,?,?,?,?,?,?,?,?,?)
        """, event.tenantId(), event.runId(), event.attemptId(), request.toolCallId(),
        request.command().delegationId(), request.command().role(), request.command().input(),
        request.command().maxTokens(), request.command().maxCostCents(),
        request.command().maxDurationSeconds(), request.bindingDigest(), event.eventId(),
        event.sequence());
    if (inserted != 1) {
      throw new IllegalStateException("subagent request was not persisted");
    }
  }

  private void completeSubagentResult(RunEventMessage event) {
    var calls = jdbc.query("""
        select parent_run_id,tool_call_id,delegation_id,binding_digest
          from subagent_calls
         where tenant_id = ? and child_run_id = ? and state = 'child_queued'
         for update
        """, (row, rowNumber) -> new SubagentResultBinding(
            row.getObject("parent_run_id", UUID.class), row.getString("tool_call_id"),
            row.getObject("delegation_id", UUID.class), row.getString("binding_digest")),
        event.tenantId(), event.runId());
    if (calls.isEmpty()) {
      return;
    }
    if (calls.size() != 1) {
      throw new IllegalStateException("child run has multiple active subagent bindings");
    }
    var binding = calls.getFirst();
    var result = subagentResultContent(event);
    var isError = !"succeeded".equals(event.terminalStatus());
    var digest = subagentResultDigest(binding, event, result, isError);
    var updated = jdbc.update("""
        update subagent_calls
           set state = 'result_ready', child_terminal_event_id = ?, terminal_status = ?,
               result = ?::jsonb, result_digest = ?, result_is_error = ?,
               updated_at = clock_timestamp()
         where tenant_id = ? and parent_run_id = ? and tool_call_id = ?
           and child_run_id = ? and state = 'child_queued'
        """, event.eventId(), event.terminalStatus(), result.toString(), digest, isError,
        event.tenantId(), binding.parentRunId(), binding.toolCallId(), event.runId());
    if (updated != 1) {
      throw new IllegalStateException("child terminal result lost its parent binding");
    }
  }

  private JsonNode subagentResultContent(RunEventMessage event) {
    if (!"succeeded".equals(event.terminalStatus())) {
      var result = JSON.createObjectNode();
      result.put("terminal_status", event.terminalStatus());
      try {
        result.set("error", JSON.readTree(event.payload()));
      } catch (JsonProcessingException exception) {
        throw new IllegalArgumentException("terminal event payload is malformed", exception);
      }
      return result;
    }
    var output = jdbc.queryForObject("""
        select coalesce(string_agg(payload->>'text','' order by sequence),'')
          from run_events
         where tenant_id = ? and run_id = ? and type = 'model.output.delta'
        """, String.class, event.tenantId(), event.runId());
    var bounded = truncateUtf8(output == null ? "" : output, SUBAGENT_RESULT_TEXT_MAX_BYTES);
    var result = JSON.createObjectNode();
    result.put("text", bounded);
    if (!bounded.equals(output)) {
      result.put("truncated", true);
    }
    return result;
  }

  private String subagentResultDigest(
      SubagentResultBinding binding, RunEventMessage event, JsonNode result, boolean isError) {
    var material = JSON.createArrayNode();
    material.add(binding.toolCallId());
    material.add(binding.delegationId().toString());
    material.add(binding.bindingDigest());
    material.add(event.runId().toString());
    material.add(event.eventId().toString());
    material.add(event.terminalStatus());
    material.add(result);
    material.add(isError);
    return sha256(material.toString());
  }

  private String truncateUtf8(String value, int maxBytes) {
    var bytes = value.getBytes(StandardCharsets.UTF_8);
    if (bytes.length <= maxBytes) {
      return value;
    }
    var end = maxBytes;
    while (end > 0 && (bytes[end] & 0xc0) == 0x80) {
      end--;
    }
    return new String(bytes, 0, end, StandardCharsets.UTF_8);
  }

  private void deliverSubagentResult(RunEventMessage event) {
    try {
      var payload = JSON.readTree(event.payload());
      var updated = jdbc.update("""
          update subagent_calls
             set state = 'delivered', delivered_event_id = ?, updated_at = clock_timestamp()
           where tenant_id = ? and parent_run_id = ? and delivery_attempt_id = ?
             and tool_call_id = ? and delegation_id = ? and binding_digest = ?
             and child_run_id = ? and child_terminal_event_id = ?
             and result_digest = ? and state = 'result_ready'
          """, event.eventId(), event.tenantId(), event.runId(), event.attemptId(),
          payload.path("tool_call_id").asText(),
          UUID.fromString(payload.path("delegation_id").asText()),
          payload.path("binding_digest").asText(),
          UUID.fromString(payload.path("child_run_id").asText()),
          UUID.fromString(payload.path("child_terminal_event_id").asText()),
          payload.path("result_digest").asText());
      if (updated != 1) {
        throw new IllegalStateException("subagent result receipt lost its durable binding");
      }
      updated = jdbc.update("""
          update runs set status = 'running', last_sequence = ?, updated_at = clock_timestamp()
           where tenant_id = ? and id = ? and current_attempt_id = ? and status = 'suspended'
          """, event.sequence(), event.tenantId(), event.runId(), event.attemptId());
      if (updated != 1) {
        throw new IllegalStateException("subagent result receipt lost parent run ownership");
      }
    } catch (JsonProcessingException | IllegalArgumentException exception) {
      throw new IllegalArgumentException("subagent result event payload is malformed", exception);
    }
  }

  private PendingSubagentRequest parseSubagentRequest(String payload) {
    try {
      var root = JSON.readTree(payload);
      var request = root.path("request");
      var toolCallId = request.path("tool_call_id").asText();
      var bindingDigest = request.path("binding_digest").asText();
      if (!"suspended".equals(root.path("status").asText())
          || toolCallId.isBlank() || toolCallId.length() > 256
          || !isSha256(bindingDigest)) {
        throw new IllegalArgumentException("subagent request identity is invalid");
      }
      var budget = request.path("budget");
      return new PendingSubagentRequest(
          toolCallId,
          bindingDigest,
          new SpawnSubagentCommand(
              UUID.fromString(request.path("delegation_id").asText()),
              request.path("role").asText(),
              request.path("input").asText(),
              budget.path("max_tokens").longValue(),
              budget.path("max_cost_cents").longValue(),
              budget.path("max_duration_seconds").longValue()));
    } catch (JsonProcessingException | IllegalArgumentException exception) {
      throw new IllegalArgumentException("subagent request payload is malformed", exception);
    }
  }

  private void persistApproval(RunEventMessage event, ActiveRun run) {
    var request = parseApproval(event.payload());
    var inserted = jdbc.update("""
        insert into approvals (
          tenant_id,id,run_id,attempt_id,worker_id,worker_incarnation_id,
          tool_call_id,binding_digest,request,policy_snapshot,policy_digest,
          session_scope_digest,session_grant_eligible)
        values (?,?,?,?,?,?,?,?,?::jsonb,?::jsonb,?,?,?)
        """, event.tenantId(), request.approvalId(), event.runId(), event.attemptId(),
        run.workerId(), run.workerIncarnationId(), request.toolCallId(),
        request.bindingDigest(), event.payload(),
        request.scope() == null ? null : request.scope().policySnapshot().toString(),
        request.scope() == null ? null : request.scope().policyDigest(),
        request.scope() == null ? null : request.scope().sessionScopeDigest(),
        request.scope() != null && request.scope().sessionGrantEligible());
    if (inserted != 1) {
      throw new IllegalStateException("approval request was not persisted");
    }
    var updated = jdbc.update("""
        update runs
           set status = 'waiting_approval', last_sequence = ?, updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and current_attempt_id = ? and status = 'running'
        """, event.sequence(), event.tenantId(), event.runId(), event.attemptId());
    if (updated != 1) {
      throw new IllegalStateException("approval event lost its active run ownership");
    }
    if (request.scope() != null && request.scope().sessionGrantEligible()) {
      autoApproveFromSessionGrant(event, run, request);
    }
  }

  private void autoApproveFromSessionGrant(
      RunEventMessage event, ActiveRun run, ApprovalRequest request) {
    var grants = jdbc.query("""
        select g.id
          from session_tool_grants g
          join sessions s on s.tenant_id = g.tenant_id and s.id = g.session_id
         where g.tenant_id = ? and g.application_id = ? and g.session_id = ?
           and g.workspace_id = ? and g.agent_version_id = ?
           and g.scope_digest = ? and g.policy_digest = ? and s.state = 'active'
           and g.tool_name = ? and g.effect = ? and g.sandbox = ?
           and g.policy_snapshot = ?::jsonb
         limit 1
        """, (row, rowNumber) -> row.getObject("id", UUID.class),
        event.tenantId(), run.applicationId(), run.sessionId(), run.workspaceId(),
        run.agentVersionId(), request.scope().sessionScopeDigest(),
        request.scope().policyDigest(), request.scope().toolName(), request.scope().effect(),
        request.scope().sandbox(), request.scope().policySnapshot().toString());
    if (grants.isEmpty()) {
      return;
    }
    var decidedAt = event.timestamp();
    var updated = jdbc.update("""
        update approvals
           set version = 2, status = 'approved',
               decision = jsonb_build_object(
                 'decision','allow_session','reason','matched_session_grant'),
               decided_by = ?, decided_at = ?
         where tenant_id = ? and id = ? and version = 1 and status = 'pending'
        """, "session-grant:" + grants.getFirst(), Timestamp.from(decidedAt),
        event.tenantId(), request.approvalId());
    if (updated != 1) {
      throw new IllegalStateException("session grant lost its pending approval binding");
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
          'approval_version',2,
          'binding_digest',?,
          'decision','allow_once',
          'issued_at',?,
          'expires_at',?))
        """, event.tenantId(), messageId, event.runId(), messageId.toString(),
        event.tenantId().toString(), event.runId().toString(), event.attemptId().toString(),
        run.workerId().toString(), run.workerIncarnationId().toString(),
        request.approvalId().toString(), request.bindingDigest(), decidedAt.toString(),
        decidedAt.plusSeconds(300).toString());
  }

  private ApprovalRequest parseApproval(String payload) {
    try {
      var root = JSON.readTree(payload);
      var approval = root.path("approval");
      var execution = approval.path("execution");
      var approvalId = UUID.fromString(approval.path("approval_id").asText());
      var toolCallId = execution.path("call").path("id").asText();
      var bindingDigest = execution.path("binding_digest").asText();
      if (toolCallId.isBlank() || toolCallId.length() > 256
          || !bindingDigest.matches("[0-9a-f]{64}")) {
        throw new IllegalArgumentException("approval event has an invalid tool binding");
      }
      var scope = ToolApprovalScope.parse(approval).orElse(null);
      return new ApprovalRequest(approvalId, toolCallId, bindingDigest, scope);
    } catch (JsonProcessingException | IllegalArgumentException exception) {
      throw new IllegalArgumentException("approval event payload is malformed", exception);
    }
  }

  private void finishRun(RunEventMessage event, ActiveRun run) {
    var updated = jdbc.update("""
        update runs
           set status = ?, current_attempt_id = null, last_sequence = ?, finished_at = ?,
               updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and current_attempt_id = ?
           and status in ('running','waiting_approval','suspended')
        """, event.terminalStatus(), event.sequence(), Timestamp.from(event.timestamp()),
        event.tenantId(), event.runId(), event.attemptId());
    if (updated != 1) {
      throw new IllegalStateException("terminal event lost its active run ownership");
    }
    rejectPendingSteering(event.tenantId(), event.runId(), "run_terminated");
    jdbc.update("""
        update run_dispatches set state = 'finished', updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and attempt_id = ?
           and state in ('accepted','suspended')
        """, event.tenantId(), event.runId(), event.attemptId());
    jdbc.update("""
        update runtime_workers
           set active_runs = greatest(active_runs - 1, 0), updated_at = clock_timestamp()
         where id = ? and current_incarnation_id = ?
        """, run.workerId(), run.workerIncarnationId());
    jdbc.update("""
        update runtime_worker_incarnations
           set active_runs = greatest(active_runs - 1, 0), updated_at = clock_timestamp()
         where worker_id = ? and incarnation_id = ?
        """, run.workerId(), run.workerIncarnationId());
    jdbc.update("""
        update workspace_leases
           set expires_at = clock_timestamp(), updated_at = clock_timestamp()
         where tenant_id = ? and workspace_id = ? and owner_id = ?
           and owner_epoch = ? and fencing_token = ?
        """, event.tenantId(), run.workspaceId(), run.workerId(), run.ownerEpoch(),
        run.fencingToken());
    jdbc.update("""
        update workspaces set state = 'ready', updated_at = clock_timestamp()
         where tenant_id = ? and id = ?
           and not exists (
             select 1 from workspace_leases l
              where l.tenant_id = workspaces.tenant_id and l.workspace_id = workspaces.id
                and l.expires_at > clock_timestamp())
        """, event.tenantId(), run.workspaceId());
  }

  private void rejectPendingSteering(UUID tenantId, UUID runId, String reason) {
    jdbc.update("""
        update run_steering_commands
           set state = 'rejected', rejection_reason = ?, rejected_at = clock_timestamp(),
               updated_at = clock_timestamp()
         where tenant_id = ? and run_id = ? and state = 'pending'
        """, reason, tenantId, runId);
  }

  public ScheduleResult schedule(
      UUID tenantId, UUID runId, Duration leaseDuration, Duration heartbeatFreshness) {
    validateDuration(leaseDuration, "lease duration", Duration.ofMinutes(5));
    validateDuration(heartbeatFreshness, "heartbeat freshness", Duration.ofMinutes(5));
    return transactions.execute(status -> scheduleInTransaction(
        tenantId, runId, leaseDuration, heartbeatFreshness));
  }

  private ScheduleResult scheduleInTransaction(
      UUID tenantId, UUID runId, Duration leaseDuration, Duration heartbeatFreshness) {
    setTenant(tenantId);
    var existing = findDispatch(tenantId, runId);
    if (!existing.isEmpty()) {
      return ScheduleResult.withCommand(ScheduleStatus.ALREADY_DISPATCHED, existing.getFirst());
    }

    var runs = jdbc.query("""
        select r.tenant_id,r.id,r.session_id,r.workspace_id,r.agent_version_id,r.model_policy_id,
               r.input,r.status,r.placement,r.max_tokens,r.max_cost_cents,r.max_duration_seconds,
               coalesce(r.root_run_id,r.id) as root_run_id,r.parent_run_id,r.delegation_id,
               r.subagent_depth,r.agent_role,
               case when r.subagent_depth = 0
                 then coalesce(av.spec->>'instructions','')
                 else concat(coalesce(av.spec->>'instructions',''),
                   E'\n\n[Subagent role ',r.agent_role,E']\n',sr.role->>'instructions')
               end as agent_instructions,
               array(select jsonb_array_elements_text(
                 case when r.subagent_depth = 0
                   then coalesce(av.spec->'delegated_scopes','[]'::jsonb)
                   else coalesce(sr.role->'delegated_scopes','[]'::jsonb)
                 end) order by 1) as delegated_scopes
          from runs r
          join agent_versions av on av.tenant_id = r.tenant_id and av.id = r.agent_version_id
          left join lateral (
            select role_value as role from jsonb_array_elements(
              coalesce(av.spec->'subagent_roles','[]'::jsonb)) as roles(role_value)
             where role_value->>'name' = r.agent_role limit 1
          ) sr on r.subagent_depth > 0
         where r.tenant_id = ? and r.id = ?
           and (r.subagent_depth = 0 or sr.role is not null)
         for update of r
        """, (row, rowNumber) -> new SchedulableRun(
            row.getObject("tenant_id", UUID.class),
            row.getObject("id", UUID.class),
            row.getObject("session_id", UUID.class),
            row.getObject("workspace_id", UUID.class),
            row.getObject("agent_version_id", UUID.class),
            row.getObject("model_policy_id", UUID.class),
            row.getString("input"),
            row.getString("status"),
            row.getString("placement"),
            stringList(row, "delegated_scopes"),
            row.getString("agent_instructions"),
            row.getObject("root_run_id", UUID.class),
            row.getObject("parent_run_id", UUID.class),
            row.getObject("delegation_id", UUID.class),
            row.getInt("subagent_depth"),
            row.getString("agent_role"),
            row.getLong("max_tokens"),
            row.getLong("max_cost_cents"),
            row.getLong("max_duration_seconds")), tenantId, runId);
    if (runs.isEmpty() || !"queued".equals(runs.getFirst().status())) {
      return ScheduleResult.withoutCommand(ScheduleStatus.IGNORED_NOT_QUEUED);
    }
    var run = runs.getFirst();
    var workers = jdbc.query("""
        select i.worker_id,i.incarnation_id
          from runtime_workers w
          join runtime_worker_incarnations i
            on i.worker_id = w.id and i.incarnation_id = w.current_incarnation_id
         where i.last_heartbeat_received_at >=
           clock_timestamp() - (? * interval '1 millisecond')
           and i.accepting_work
           and i.active_runs < i.capacity
           and (? = 'any' or ? = any(i.placements))
         order by (i.active_runs::numeric / i.capacity), i.last_heartbeat_received_at desc,
                  i.worker_id,i.incarnation_id
         for update skip locked
         limit 1
        """, (row, rowNumber) -> new WorkerTarget(
            row.getObject("worker_id", UUID.class),
            row.getObject("incarnation_id", UUID.class)),
        heartbeatFreshness.toMillis(), run.placement(), run.placement());
    if (workers.isEmpty()) {
      return ScheduleResult.withoutCommand(ScheduleStatus.RETRY_NO_CAPACITY);
    }

    var worker = workers.getFirst();
    var attemptId = UUID.randomUUID();
    var fencingToken = UUID.randomUUID();
    var leases = jdbc.query("""
        insert into workspace_leases (
          tenant_id,workspace_id,owner_id,owner_epoch,fencing_token,expires_at)
        values (?,?,?,1,?,clock_timestamp() + (? * interval '1 millisecond'))
        on conflict (tenant_id,workspace_id) do update
           set owner_id = excluded.owner_id,
               owner_epoch = workspace_leases.owner_epoch + 1,
               fencing_token = excluded.fencing_token,
               expires_at = excluded.expires_at,
               updated_at = clock_timestamp()
         where workspace_leases.expires_at <= clock_timestamp()
        returning owner_epoch,fencing_token,expires_at
        """, (row, rowNumber) -> new LeaseResult(
            row.getLong("owner_epoch"),
            row.getObject("fencing_token", UUID.class),
            row.getTimestamp("expires_at").toInstant()),
        tenantId, run.workspaceId(), worker.workerId(), fencingToken, leaseDuration.toMillis());
    if (leases.isEmpty()) {
      return ScheduleResult.withoutCommand(ScheduleStatus.RETRY_WORKSPACE_BUSY);
    }

    var lease = leases.getFirst();
    var issuedAt = java.time.Instant.now();
    var messageId = UUID.randomUUID();
    var modelPolicySnapshot = modelPolicySnapshot(tenantId, run.modelPolicyId());
    var skillSnapshots = skillSnapshots(tenantId, run.agentVersionId());
    var workloadToken = workloadTokenIssuer.issue(new WorkloadIdentityClaims(
        tenantId, run.id(), attemptId, worker.workerId(), worker.incarnationId(),
        run.modelPolicyId(), modelPolicySnapshot.digest(), issuedAt,
        earliest(lease.expiresAt(), issuedAt.plus(Duration.ofMinutes(5)))));
    var command = new RunExecutionCommand(
        EXECUTION_SCHEMA_VERSION,
        messageId, tenantId,
        run.id(), run.sessionId(), run.workspaceId(),
        run.agentVersionId(), run.modelPolicyId(), attemptId, worker.workerId(),
        worker.incarnationId(), lease.ownerEpoch(), lease.fencingToken(),
        issuedAt, lease.expiresAt(), workloadToken, run.delegatedScopes(),
        run.agentInstructions(), modelPolicySnapshot.base64(), modelPolicySnapshot.digest(),
        skillSnapshots,
        run.lineage(),
        subagentRoles(tenantId, run.agentVersionId(), run.delegatedScopes(), run.subagentDepth()),
        run.input(), run.maxTokens(), run.maxCostCents(),
        run.maxDurationSeconds());

    jdbc.update("""
        insert into run_dispatches (
          tenant_id,run_id,attempt_id,worker_id,worker_incarnation_id,owner_epoch,fencing_token,
          lease_expires_at,workload_identity_expires_at,state,requested_at)
        values (?,?,?,?,?,?,?,?,?,'requested',?)
        """, tenantId, run.id(), attemptId, worker.workerId(), worker.incarnationId(),
        lease.ownerEpoch(), lease.fencingToken(),
        Timestamp.from(lease.expiresAt()), Timestamp.from(lease.expiresAt()),
        Timestamp.from(issuedAt));
    jdbc.update("""
        update runs
           set current_attempt_id = ?, updated_at = clock_timestamp()
         where tenant_id = ? and id = ? and status = 'queued'
        """, attemptId, tenantId, run.id());
    jdbc.update("""
        update workspaces set state = 'leased', updated_at = clock_timestamp()
         where tenant_id = ? and id = ?
        """, tenantId, run.workspaceId());
    jdbc.update("""
        update runtime_workers set active_runs = active_runs + 1, updated_at = clock_timestamp()
         where id = ? and current_incarnation_id = ? and accepting_work and active_runs < capacity
        """, worker.workerId(), worker.incarnationId());
    jdbc.update("""
        update runtime_worker_incarnations
           set active_runs = active_runs + 1, updated_at = clock_timestamp()
         where worker_id = ? and incarnation_id = ? and accepting_work and active_runs < capacity
        """, worker.workerId(), worker.incarnationId());
    insertExecutionOutbox(command);
    return ScheduleResult.withCommand(ScheduleStatus.DISPATCHED, command);
  }

  private List<RunExecutionCommand> findDispatch(UUID tenantId, UUID runId) {
    return jdbc.query("""
        select d.tenant_id,d.run_id,d.attempt_id,d.worker_id,d.worker_incarnation_id,
               d.owner_epoch,d.fencing_token,
               d.lease_expires_at,d.requested_at,r.session_id,r.workspace_id,r.agent_version_id,
               r.model_policy_id,r.input,r.max_tokens,r.max_cost_cents,r.max_duration_seconds,
               coalesce(r.root_run_id,r.id) as root_run_id,r.parent_run_id,r.delegation_id,
               r.subagent_depth,r.agent_role,
               coalesce(o.payload->>'agent_instructions','') as agent_instructions,
               coalesce(o.payload->>'model_policy_snapshot_base64','') as model_policy_snapshot_base64,
               coalesce(o.payload->>'model_policy_digest','') as model_policy_digest,
               coalesce((o.payload->>'schema_version')::int,2) as execution_schema_version,
               coalesce(o.payload->'skill_snapshots','[]'::jsonb)::text as skill_snapshots,
               coalesce(o.payload->'subagent_roles','[]'::jsonb)::text as subagent_roles,
               o.id as message_id,o.payload->>'workload_token' as workload_token,
               array(select jsonb_array_elements_text(o.payload->'delegated_scopes') order by 1)
                 as delegated_scopes
          from run_dispatches d
          join runs r on r.tenant_id = d.tenant_id and r.id = d.run_id
          join outbox_events o on o.tenant_id = d.tenant_id and o.aggregate_id = d.run_id
                              and o.event_type = 'run.execution.requested'
                              and (o.payload->>'attempt_id')::uuid = d.attempt_id
         where d.tenant_id = ? and d.run_id = ? and d.attempt_id = r.current_attempt_id
           and d.state in ('requested', 'accepted')
        """, this::mapCommand, tenantId, runId);
  }

  private RunExecutionCommand mapCommand(ResultSet row, int rowNumber) throws SQLException {
    return new RunExecutionCommand(
        row.getInt("execution_schema_version"),
        row.getObject("message_id", UUID.class),
        row.getObject("tenant_id", UUID.class),
        row.getObject("run_id", UUID.class),
        row.getObject("session_id", UUID.class),
        row.getObject("workspace_id", UUID.class),
        row.getObject("agent_version_id", UUID.class),
        row.getObject("model_policy_id", UUID.class),
        row.getObject("attempt_id", UUID.class),
        row.getObject("worker_id", UUID.class),
        row.getObject("worker_incarnation_id", UUID.class),
        row.getLong("owner_epoch"),
        row.getObject("fencing_token", UUID.class),
        row.getTimestamp("requested_at").toInstant(),
        row.getTimestamp("lease_expires_at").toInstant(),
        new WorkloadToken(row.getString("workload_token")),
        stringList(row, "delegated_scopes"),
        row.getString("agent_instructions"),
        row.getString("model_policy_snapshot_base64"),
        row.getString("model_policy_digest"),
        parseSkillSnapshots(row.getString("skill_snapshots")),
        new AgentLineageSnapshot(
            row.getObject("root_run_id", UUID.class),
            row.getObject("parent_run_id", UUID.class),
            row.getObject("delegation_id", UUID.class),
            row.getInt("subagent_depth"),
            row.getString("agent_role")),
        parseSubagentRoles(row.getString("subagent_roles")),
        row.getString("input"),
        row.getLong("max_tokens"),
        row.getLong("max_cost_cents"),
        row.getLong("max_duration_seconds"));
  }

  private void insertExecutionOutbox(RunExecutionCommand command) {
    jdbc.update("""
        insert into outbox_events (
          tenant_id,id,aggregate_type,aggregate_id,event_type,payload)
        values (?,?,'run',?,'run.execution.requested',jsonb_build_object(
          'schema_version',?,
          'message_id',?,
          'tenant_id',?,
          'run_id',?,
          'session_id',?,
          'workspace_id',?,
          'agent_version_id',?,
          'model_policy_id',?,
          'attempt_id',?,
          'worker_id',?,
          'worker_incarnation_id',?,
          'owner_epoch',?,
          'fencing_token',?,
          'issued_at',?,
          'lease_expires_at',?,
          'workload_token',?,
          'delegated_scopes',to_jsonb(?::text[]),
          'agent_instructions',?,
          'model_policy_snapshot_base64',?,
          'model_policy_digest',?,
          'skill_snapshots',?::jsonb,
          'subagent_roles',?::jsonb,
          'lineage',jsonb_build_object(
            'root_run_id',?::text,
            'parent_run_id',?::text,
            'delegation_id',?::text,
            'depth',?::integer,
            'role',?::text
          ),
          'input',?,
          'budget',jsonb_build_object(
            'max_tokens',?,
            'max_cost_cents',?,
            'max_duration_seconds',?
          )
        ))
        """, command.tenantId(), command.messageId(), command.runId(), command.schemaVersion(),
        command.messageId().toString(), command.tenantId().toString(), command.runId().toString(),
        command.sessionId().toString(), command.workspaceId().toString(),
        command.agentVersionId().toString(), command.modelPolicyId().toString(), command.attemptId().toString(),
        command.workerId().toString(), command.workerIncarnationId().toString(),
        command.ownerEpoch(), command.fencingToken().toString(),
        command.issuedAt().toString(), command.leaseExpiresAt().toString(),
        command.workloadToken().value(), command.delegatedScopes().toArray(String[]::new),
        command.agentInstructions(), command.modelPolicySnapshotBase64(),
        command.modelPolicyDigest(), skillSnapshotsNode(command.skillSnapshots()).toString(),
        subagentRolesNode(command.subagentRoles()).toString(),
        command.lineage().rootRunId().toString(), nullableUuid(command.lineage().parentRunId()),
        nullableUuid(command.lineage().delegationId()), command.lineage().depth(),
        command.lineage().role(),
        command.input(),
        command.maxTokens(), command.maxCostCents(), command.maxDurationSeconds());
  }

  private void writeLineage(
      com.fasterxml.jackson.databind.node.ObjectNode target, AgentLineageSnapshot lineage) {
    var node = target.putObject("lineage");
    node.put("root_run_id", lineage.rootRunId().toString());
    if (lineage.parentRunId() == null) node.putNull("parent_run_id");
    else node.put("parent_run_id", lineage.parentRunId().toString());
    if (lineage.delegationId() == null) node.putNull("delegation_id");
    else node.put("delegation_id", lineage.delegationId().toString());
    node.put("depth", lineage.depth());
    node.put("role", lineage.role());
  }

  private String nullableUuid(UUID value) {
    return value == null ? null : value.toString();
  }

  private JsonNode skillSnapshotsNode(List<SkillSnapshot> skillSnapshots) {
    var result = JSON.createArrayNode();
    for (var skill : skillSnapshots) {
      var item = result.addObject();
      item.put("schema_version", skill.schemaVersion());
      item.put("application_id", skill.applicationId().toString());
      item.put("skill_version_id", skill.skillVersionId().toString());
      item.put("name", skill.name());
      item.put("semantic_version", skill.semanticVersion());
      item.put("description", skill.description());
      item.put("instructions", skill.instructions());
      var tools = item.putArray("tool_names");
      skill.toolNames().forEach(tools::add);
      var platforms = item.putArray("supported_platforms");
      skill.supportedPlatforms().forEach(platforms::add);
      item.put("min_runtime_version", skill.minRuntimeVersion());
      item.put("artifact_digest", skill.artifactDigest());
      item.put("signing_key_id", skill.signingKeyId());
      item.put("signature", skill.signature());
    }
    return result;
  }

  private JsonNode subagentRolesNode(List<SubagentRoleSnapshot> roles) {
    var result = JSON.createArrayNode();
    for (var role : roles) {
      var item = result.addObject();
      item.put("name", role.name());
      item.put("instructions", role.instructions());
      var scopes = item.putArray("delegated_scopes");
      role.delegatedScopes().forEach(scopes::add);
    }
    return result;
  }

  private List<SubagentRoleSnapshot> subagentRoles(
      UUID tenantId, UUID agentVersionId, List<String> currentScopes, int currentDepth) {
    if (currentDepth >= 3 || !currentScopes.contains("agent:spawn")) {
      return List.of();
    }
    return jdbc.query("""
        select role::text
          from agent_versions av
          cross join lateral jsonb_array_elements(
            coalesce(av.spec->'subagent_roles','[]'::jsonb)) role
         where av.tenant_id = ? and av.id = ?
         order by role->>'name'
        """, (row, rowNumber) -> parseSubagentRole(row.getString(1)), tenantId, agentVersionId)
        .stream()
        .filter(role -> currentScopes.containsAll(role.delegatedScopes()))
        .toList();
  }

  private SubagentRoleSnapshot parseSubagentRole(String encoded) {
    try {
      var role = JSON.readTree(encoded);
      return new SubagentRoleSnapshot(
          role.path("name").asText(), role.path("instructions").asText(),
          jsonStringList(role.path("delegated_scopes")));
    } catch (JsonProcessingException | IllegalArgumentException invalidRole) {
      throw new IllegalStateException("stored subagent role is invalid", invalidRole);
    }
  }

  private List<SubagentRoleSnapshot> parseSubagentRoles(String encoded) {
    try {
      var root = JSON.readTree(encoded);
      if (!root.isArray()) {
        throw new IllegalArgumentException("execution subagent role snapshots are invalid");
      }
      var result = new ArrayList<SubagentRoleSnapshot>();
      for (var item : root) {
        result.add(new SubagentRoleSnapshot(
            item.path("name").asText(), item.path("instructions").asText(),
            jsonStringList(item.path("delegated_scopes"))));
      }
      return List.copyOf(result);
    } catch (JsonProcessingException | IllegalArgumentException invalidSnapshot) {
      throw new IllegalStateException("execution subagent role snapshots are invalid", invalidSnapshot);
    }
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject("select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }

  private void validateDuration(Duration value, String name, Duration maximum) {
    if (value == null || value.isZero() || value.isNegative() || value.compareTo(maximum) > 0) {
      throw new IllegalArgumentException(name + " must be between 1ms and " + maximum.toSeconds() + " seconds");
    }
  }

  private Instant earliest(Instant first, Instant second) {
    return first.isBefore(second) ? first : second;
  }

  private List<SkillSnapshot> skillSnapshots(UUID tenantId, UUID agentVersionId) {
    return jdbc.query("""
        select sv.artifact::text as artifact,sv.artifact_digest,
               sv.signing_key_id,sv.signature
          from agent_version_skills avs
          join skill_versions sv
            on sv.tenant_id = avs.tenant_id
           and sv.application_id = avs.application_id
           and sv.id = avs.skill_version_id
           and sv.artifact_digest = avs.artifact_digest
         where avs.tenant_id = ? and avs.agent_version_id = ?
         order by avs.ordinal
        """, (row, rowNumber) -> {
      try {
        var artifact = JSON.readTree(row.getString("artifact"));
        return new SkillSnapshot(
            artifact.path("schema_version").asInt(),
            UUID.fromString(artifact.path("application_id").asText()),
            UUID.fromString(artifact.path("skill_version_id").asText()),
            artifact.path("name").asText(), artifact.path("semantic_version").asText(),
            artifact.path("description").asText(), artifact.path("instructions").asText(),
            jsonStringList(artifact.path("tool_names")),
            jsonStringList(artifact.path("supported_platforms")),
            artifact.path("min_runtime_version").asText(),
            row.getString("artifact_digest"), row.getString("signing_key_id"),
            row.getString("signature"));
      } catch (JsonProcessingException | IllegalArgumentException invalidArtifact) {
        throw new IllegalStateException("stored Skill artifact is invalid", invalidArtifact);
      }
    }, tenantId, agentVersionId);
  }

  private List<String> jsonStringList(JsonNode value) {
    if (!value.isArray()) {
      throw new IllegalArgumentException("Skill artifact list is invalid");
    }
    var result = new ArrayList<String>();
    value.forEach(item -> result.add(item.asText()));
    return List.copyOf(result);
  }

  private List<SkillSnapshot> parseSkillSnapshots(String encoded) {
    try {
      var root = JSON.readTree(encoded);
      if (!root.isArray()) {
        throw new IllegalArgumentException("execution Skill snapshots are invalid");
      }
      var result = new ArrayList<SkillSnapshot>();
      for (var item : root) {
        result.add(new SkillSnapshot(
            item.path("schema_version").asInt(),
            UUID.fromString(item.path("application_id").asText()),
            UUID.fromString(item.path("skill_version_id").asText()),
            item.path("name").asText(), item.path("semantic_version").asText(),
            item.path("description").asText(), item.path("instructions").asText(),
            jsonStringList(item.path("tool_names")),
            jsonStringList(item.path("supported_platforms")),
            item.path("min_runtime_version").asText(), item.path("artifact_digest").asText(),
            item.path("signing_key_id").asText(), item.path("signature").asText()));
      }
      return List.copyOf(result);
    } catch (JsonProcessingException | IllegalArgumentException invalidSnapshot) {
      throw new IllegalStateException("execution Skill snapshots are invalid", invalidSnapshot);
    }
  }

  private ModelPolicySnapshot modelPolicySnapshot(UUID tenantId, UUID modelPolicyId) {
    var rows = jdbc.query("""
        select mp.policy->>'routing' as routing,c.priority,p.id as provider_id,
               p.protocol,p.endpoint,p.model,p.credential_envelope::text as credential_envelope
          from model_policies mp
          left join model_policy_candidates c
            on c.tenant_id = mp.tenant_id and c.model_policy_id = mp.id
          left join model_providers p
            on p.tenant_id = c.tenant_id and p.id = c.provider_id and p.state = 'active'
         where mp.tenant_id = ? and mp.id = ?
         order by c.priority
        """, (row, rowNumber) -> new ProviderSnapshotRow(
            row.getString("routing"), row.getObject("provider_id", UUID.class),
            row.getString("protocol"), row.getString("endpoint"), row.getString("model"),
            row.getString("credential_envelope")), tenantId, modelPolicyId);
    if (rows.isEmpty() || rows.getFirst().providerId() == null) {
      return ModelPolicySnapshot.empty();
    }
    try {
      var snapshot = JSON.createObjectNode();
      snapshot.put("schema_version", 1);
      snapshot.put("routing", rows.getFirst().routing());
      var candidates = snapshot.putArray("candidates");
      for (var row : rows) {
        if (row.providerId() == null || row.credentialEnvelope() == null) {
          throw new IllegalStateException("model policy references an unavailable provider");
        }
        var candidate = candidates.addObject();
        candidate.put("provider_id", row.providerId().toString());
        candidate.put("protocol", row.protocol());
        candidate.put("endpoint", row.endpoint());
        candidate.put("model", row.model());
        candidate.set("credential_envelope", JSON.readTree(row.credentialEnvelope()));
      }
      var bytes = JSON.writeValueAsBytes(snapshot);
      return new ModelPolicySnapshot(
          Base64.getEncoder().encodeToString(bytes), sha256(bytes));
    } catch (JsonProcessingException invalidProviderSnapshot) {
      throw new IllegalStateException(
          "model policy provider snapshot could not be serialized", invalidProviderSnapshot);
    }
  }

  private Timestamp timestamp(Instant value) {
    return value == null ? null : Timestamp.from(value);
  }

  private record SchedulableRun(
      UUID tenantId,
      UUID id,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID modelPolicyId,
      String input,
      String status,
      String placement,
      List<String> delegatedScopes,
      String agentInstructions,
      UUID rootRunId,
      UUID parentRunId,
      UUID delegationId,
      int subagentDepth,
      String agentRole,
      long maxTokens,
      long maxCostCents,
      long maxDurationSeconds) {
    private AgentLineageSnapshot lineage() {
      return new AgentLineageSnapshot(
          rootRunId, parentRunId, delegationId, subagentDepth, agentRole);
    }
  }

  private record ProviderSnapshotRow(
      String routing,
      UUID providerId,
      String protocol,
      String endpoint,
      String model,
      String credentialEnvelope) {}

  private record ModelPolicySnapshot(String base64, String digest) {
    private static ModelPolicySnapshot empty() {
      return new ModelPolicySnapshot("", "");
    }
  }

  private List<String> stringList(ResultSet row, String column) throws SQLException {
    var array = row.getArray(column);
    if (array == null) {
      return List.of();
    }
    return List.of((String[]) array.getArray());
  }

  private record LeaseResult(long ownerEpoch, UUID fencingToken, java.time.Instant expiresAt) {}

  private record RenewableAssignment(UUID modelPolicyId) {}

  private record IdentityRenewal(long generation, Instant expiresAt) {}

  private record ActiveRun(
      long lastSequence,
      String status,
      UUID applicationId,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID workerId,
      UUID workerIncarnationId,
      long ownerEpoch,
      UUID fencingToken) {}

  private record WorkerTarget(UUID workerId, UUID incarnationId) {}

  private record ApprovalRequest(
      UUID approvalId, String toolCallId, String bindingDigest, ToolApprovalScope scope) {}

  private record PendingSubagentRequest(
      String toolCallId, String bindingDigest, SpawnSubagentCommand command) {}

  private record SteeringReceipt(UUID steeringId, String inputDigest) {}

  private record SteeringOutcomeState(
      String state, UUID outcomeMessageId, String rejectionReason) {}

  private record DurableSubagentHandoff(
      UUID applicationId,
      String toolCallId,
      String bindingDigest,
      SpawnSubagentCommand command) {}

  private record SubagentResultBinding(
      UUID parentRunId,
      String toolCallId,
      UUID delegationId,
      String bindingDigest) {}

  private record SubagentResultKey(UUID tenantId, UUID parentRunId, String toolCallId) {}

  private record DurableSubagentResult(
      String toolCallId,
      UUID delegationId,
      String bindingDigest,
      UUID childRunId,
      UUID childTerminalEventId,
      String terminalStatus,
      String content,
      boolean isError,
      String digest) {}

  private record DurableSteering(UUID steeringId, String input, String inputDigest) {}

  private record DurableApprovalDecision(
      UUID approvalId, int version, String status, String bindingDigest, String decision) {}

  private record SubagentResume(
      UUID sourceAttemptId,
      ExpiredDispatch dispatch,
      StoredCheckpoint checkpoint,
      DurableSubagentResult result) {}

  private record ToolLedgerMutation(
      ToolLedgerMutationType type,
      String toolCallId,
      String bindingDigest,
      String effect,
      String sandbox,
      String request,
      boolean denial) {

    private static ToolLedgerMutation none() {
      return new ToolLedgerMutation(
          ToolLedgerMutationType.NONE, null, null, null, null, null, false);
    }
  }

  private enum ToolLedgerMutationType {
    NONE,
    PLAN,
    START,
    COMPLETE
  }

  private record DispatchKey(UUID tenantId, UUID runId, UUID attemptId) {}

  private record ExpiredDispatch(
      String state,
      UUID workerId,
      UUID workerIncarnationId,
      Instant lastConfirmedHealthyAt,
      long ownerEpoch,
      UUID fencingToken,
      UUID sessionId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID modelPolicyId,
      String input,
      String runStatus,
      String placement,
      long lastSequence,
      List<String> delegatedScopes,
      String agentInstructions,
      UUID rootRunId,
      UUID parentRunId,
      UUID delegationId,
      int subagentDepth,
      String agentRole,
      long maxTokens,
      long maxCostCents,
      long maxDurationSeconds) {
    private AgentLineageSnapshot lineage() {
      return new AgentLineageSnapshot(
          rootRunId, parentRunId, delegationId, subagentDepth, agentRole);
    }
  }

  private record StoredCheckpoint(
      UUID checkpointId,
      long ownerEpoch,
      UUID fencingToken,
      long sequence,
      String status,
      String kernelDigest,
      String toolCatalogDigest,
      byte[] payload,
      String payloadRef,
      String payloadEncoding,
      String payloadDigest,
      String storedPayloadDigest,
      long uncompressedSize,
      long storedSize,
      Instant createdAt) {}

  private record AmbiguousToolExecution(
      String toolCallId,
      String bindingDigest,
      String effect) {}

  private record CheckpointRecoveryState(
      long sequence,
      String status,
      boolean sameEpoch,
      boolean sameFence,
      long lastSequence,
      String runStatus) {}

  private enum ReconcileOutcome {
    NONE,
    REQUEUED,
    RECOVERED,
    INDETERMINATE,
    FAILED
  }
}
