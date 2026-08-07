package com.agentplatform.control.persistence;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.agentplatform.control.run.Run;
import com.agentplatform.control.run.RunTargetNotFound;
import com.agentplatform.control.run.RunStatus;
import com.agentplatform.control.run.RunSteeringConflict;
import com.agentplatform.control.run.RunSteeringNotAllowed;
import com.agentplatform.control.event.JdbcRunEventRepository;
import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import com.agentplatform.control.workspace.JdbcWorkspaceLeaseRepository;
import com.agentplatform.control.workspace.WorkspaceAlreadyLeased;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.Timestamp;
import java.time.Instant;
import java.time.Duration;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.jdbc.datasource.DriverManagerDataSource;
import org.springframework.transaction.support.TransactionTemplate;

class JdbcRunRepositoryIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("jdbc-run-repository");

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void savingRunAndOutboxIsAtomicAndIdempotent() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var repository = new JdbcRunRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var run = new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(),
        "idempotent-request", "hello", RunStatus.QUEUED, 1000, 100, 60, Instant.now());

    var first = repository.save(ids.applicationId(), run);
    var duplicate = repository.save(ids.applicationId(), run);

    assertThat(duplicate).isEqualTo(first);
    assertThat(jdbc.queryForObject(
        "select count(*) from runs where tenant_id = ?", Integer.class, ids.tenantId())).isOne();
    assertThat(jdbc.queryForObject(
        "select count(*) from outbox_events where tenant_id = ?", Integer.class, ids.tenantId())).isOne();
    var outbox = jdbc.queryForMap(
        "select id, payload::text as payload from outbox_events where tenant_id = ?",
        ids.tenantId());
    var payload = new ObjectMapper().readTree((String) outbox.get("payload"));
    assertThat(payload.path("schema_version").asInt()).isOne();
    assertThat(payload.path("message_id").asText()).isEqualTo(outbox.get("id").toString());
    assertThat(payload.path("tenant_id").asText()).isEqualTo(ids.tenantId().toString());
    assertThat(payload.path("run_id").asText()).isEqualTo(run.id().toString());
    assertThat(payload.path("session_id").asText()).isEqualTo(ids.sessionId().toString());
    assertThat(payload.path("workspace_id").asText()).isEqualTo(ids.workspaceId().toString());
    assertThat(payload.path("agent_version_id").asText()).isEqualTo(ids.agentVersionId().toString());
    assertThat(payload.path("model_policy_id").asText()).isEqualTo(ids.modelPolicyId().toString());
    assertThat(payload.path("input").asText()).isEqualTo("hello");
    assertThat(payload.path("priority").asText()).isEqualTo("interactive");
    assertThat(payload.path("placement").asText()).isEqualTo("cloud");
    assertThat(payload.path("budget").path("max_tokens").asLong()).isEqualTo(1000);
    assertThat(payload.path("budget").path("max_cost_cents").asLong()).isEqualTo(100);
    assertThat(payload.path("budget").path("max_duration_seconds").asLong()).isEqualTo(60);
  }

  @Test
  void runCannotBindAModelPolicyFromAnotherWorkspace() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var repository = new JdbcRunRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    jdbc.queryForObject("select set_config('app.tenant_id', ?, false)", String.class,
        ids.tenantId().toString());
    var otherWorkspaceId = UUID.randomUUID();
    var otherPolicyId = UUID.randomUUID();
    jdbc.update("""
        insert into workspaces (tenant_id,id,project_id,name)
        select tenant_id,?,project_id,'Other workspace' from workspaces
         where tenant_id = ? and id = ?
        """, otherWorkspaceId, ids.tenantId(), ids.workspaceId());
    jdbc.update("""
        insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id)
        values (?,?,?,'Other policy','{}',?)
        """, ids.tenantId(), otherPolicyId, otherWorkspaceId, ids.applicationId());

    assertThatThrownBy(() -> repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        otherPolicyId, "wrong-workspace-policy", "hello", RunStatus.QUEUED,
        1000, 100, 60, Instant.now())))
        .isInstanceOf(RunTargetNotFound.class);
    assertThat(jdbc.queryForObject("""
        select count(*) from runs where tenant_id = ? and idempotency_key = 'wrong-workspace-policy'
        """, Integer.class, ids.tenantId())).isZero();
  }

  @Test
  void eventReplayStartsStrictlyAfterLastEventId() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var runs = new JdbcRunRepository(jdbc, transactions);
    var events = new JdbcRunEventRepository(jdbc, transactions);
    var run = runs.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(),
        "event-replay", "hello", RunStatus.QUEUED, 1000, 100, 60, Instant.now()));
    var firstEventId = UUID.randomUUID();
    var secondEventId = UUID.randomUUID();
    insertEvent(jdbc, ids, run.id(), firstEventId, 1, "run.started");
    insertEvent(jdbc, ids, run.id(), secondEventId, 2, "model.text_delta");

    var replay = events.findAfter(
        ids.tenantId(), ids.applicationId(), run.id(), firstEventId, 100);

    assertThat(replay).extracting(event -> event.eventId()).containsExactly(secondEventId);
    assertThat(replay.getFirst().sequence()).isEqualTo(2);

    var otherApplication = seedApplicationResourceChain(ids.tenantId(), "event-reader");
    assertThatThrownBy(() -> events.findAfter(
        ids.tenantId(), otherApplication.applicationId(), run.id(), null, 100))
        .isInstanceOf(com.agentplatform.control.run.RunNotFound.class);
  }

  @Test
  void cancellingUndispatchedRunCommitsTerminalEventWithoutCreatingWorkerCommand() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var repository = new JdbcRunRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var run = repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(),
        "cancel-before-dispatch", "hello", RunStatus.QUEUED, 1000, 100, 60, Instant.now()));

    var result = repository.requestCancellation(ids.tenantId(), run.id(), Instant.now());

    assertThat(result).isEqualTo(RunStatus.CANCELLED);
    assertThat(jdbc.queryForMap("""
        select status,last_sequence,current_attempt_id,finished_at
          from runs where tenant_id = ? and id = ?
        """, ids.tenantId(), run.id()))
        .containsEntry("status", "cancelled")
        .containsEntry("last_sequence", 1L)
        .containsEntry("current_attempt_id", null);
    assertThat(jdbc.queryForObject("""
        select count(*) from run_events
         where tenant_id = ? and run_id = ? and type = 'run.cancelled'
        """, Integer.class, ids.tenantId(), run.id())).isOne();
    assertThat(jdbc.queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.cancellation.requested'
        """, Integer.class, ids.tenantId(), run.id())).isZero();
  }

  @Test
  void expiredWorkspaceLeaseAdvancesEpochAndRejectsOldOwner() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var leases = new JdbcWorkspaceLeaseRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var workerA = UUID.randomUUID();
    var workerB = UUID.randomUUID();

    var first = leases.acquire(ids.tenantId(), ids.workspaceId(), workerA, Duration.ofSeconds(30));
    assertThatThrownBy(() -> leases.acquire(
        ids.tenantId(), ids.workspaceId(), workerB, Duration.ofSeconds(30)))
        .isInstanceOf(WorkspaceAlreadyLeased.class);
    jdbc.update(
        "update workspace_leases set expires_at = now() - interval '1 second' where tenant_id = ? and workspace_id = ?",
        ids.tenantId(), ids.workspaceId());

    var second = leases.acquire(ids.tenantId(), ids.workspaceId(), workerB, Duration.ofSeconds(30));

    assertThat(second.ownerEpoch()).isEqualTo(first.ownerEpoch() + 1);
    assertThat(second.fencingToken()).isNotEqualTo(first.fencingToken());
    assertThat(leases.renew(first, Duration.ofSeconds(30))).isFalse();
    assertThat(leases.renew(second, Duration.ofSeconds(30))).isTrue();
  }

  @Test
  void idempotencyAndRecentRunsAreScopedToApplication() throws Exception {
    var applicationA = seedResourceChain();
    var applicationB = seedApplicationResourceChain(applicationA.tenantId(), "B");
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var repository = new JdbcRunRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var createdAt = Instant.parse("2026-08-02T00:00:00Z");
    var runA = new Run(
        UUID.randomUUID(), applicationA.tenantId(), applicationA.sessionId(),
        applicationA.agentVersionId(), applicationA.workspaceId(), applicationA.modelPolicyId(),
        "shared-key", "application A", RunStatus.QUEUED, 1000, 100, 60, createdAt);
    var runB = new Run(
        UUID.randomUUID(), applicationB.tenantId(), applicationB.sessionId(),
        applicationB.agentVersionId(), applicationB.workspaceId(), applicationB.modelPolicyId(),
        "shared-key", "application B", RunStatus.QUEUED, 1000, 100, 60, createdAt.plusSeconds(1));

    repository.save(applicationA.applicationId(), runA);
    repository.save(applicationB.applicationId(), runB);

    assertThat(repository.findByIdempotencyKey(
        applicationA.tenantId(), applicationA.applicationId(), "shared-key"))
        .contains(runA);
    assertThat(repository.findByIdempotencyKey(
        applicationB.tenantId(), applicationB.applicationId(), "shared-key"))
        .contains(runB);
    assertThat(repository.findRecent(
        applicationA.tenantId(), applicationA.applicationId(), 10))
        .extracting(summary -> summary.id()).containsExactly(runA.id());
    assertThat(repository.findRecent(
        applicationB.tenantId(), applicationB.applicationId(), 10))
        .extracting(summary -> summary.id()).containsExactly(runB.id());
  }

  @Test
  void applicationCannotCreateRunAgainstAnotherApplicationsResources() throws Exception {
    var applicationA = seedResourceChain();
    var applicationB = seedApplicationResourceChain(applicationA.tenantId(), "B");
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var repository = new JdbcRunRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var foreignRun = new Run(
        UUID.randomUUID(), applicationA.tenantId(), applicationA.sessionId(),
        applicationA.agentVersionId(), applicationA.workspaceId(), applicationA.modelPolicyId(),
        "foreign-target", "hello", RunStatus.QUEUED, 1000, 100, 60, Instant.now());

    assertThatThrownBy(() -> repository.save(applicationB.applicationId(), foreignRun))
        .isInstanceOf(RunTargetNotFound.class);
    assertThat(jdbc.queryForObject(
        "select count(*) from runs where tenant_id = ? and id = ?",
        Integer.class, applicationA.tenantId(), foreignRun.id())).isZero();
  }

  @Test
  void steeringIsStoredWithItsTargetAndOutboxBeforeReturningAndExactReplayIsIdempotent()
      throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var repository = new JdbcRunRepository(jdbc, transactions);
    var run = repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(), "steering-target", "hello", RunStatus.QUEUED,
        1000, 100, 60, Instant.now()));
    var target = markRunning(jdbc, transactions, ids, run.id());
    var requestedAt = Instant.parse("2026-08-02T06:00:00Z");

    var first = repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-1",
        "Focus on the authorization failure first.", requestedAt);
    var duplicate = repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-1",
        "Focus on the authorization failure first.", requestedAt.plusSeconds(1));

    assertThat(duplicate).isEqualTo(first);
    assertThat(first.runId()).isEqualTo(run.id());
    assertThat(first.state()).isEqualTo("pending");
    assertThat(jdbc.queryForObject("""
        select count(*) from run_steering_commands
         where tenant_id = ? and run_id = ?
        """, Integer.class, ids.tenantId(), run.id())).isOne();
    var stored = jdbc.queryForMap("""
        select attempt_id,worker_id,worker_incarnation_id,input,input_digest,state
          from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, ids.tenantId(), first.steeringId());
    assertThat(stored)
        .containsEntry("attempt_id", target.attemptId())
        .containsEntry("worker_id", target.workerId())
        .containsEntry("worker_incarnation_id", target.workerIncarnationId())
        .containsEntry("input", "Focus on the authorization failure first.")
        .containsEntry("state", "pending")
        .containsEntry(
            "input_digest",
            "d4ab9135d40486358da5bba1d1a426cc2a76bd23c3c35f41fc07eda26f79983f");
    assertThat(jdbc.queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.steering.requested'
        """, Integer.class, ids.tenantId(), run.id())).isOne();
    var payload = new ObjectMapper().readTree(jdbc.queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.steering.requested'
        """, String.class, ids.tenantId(), run.id()));
    assertThat(payload.path("schema_version").asInt()).isOne();
    assertThat(payload.path("steering_id").asText()).isEqualTo(first.steeringId().toString());
    assertThat(payload.path("tenant_id").asText()).isEqualTo(ids.tenantId().toString());
    assertThat(payload.path("run_id").asText()).isEqualTo(run.id().toString());
    assertThat(payload.path("attempt_id").asText()).isEqualTo(target.attemptId().toString());
    assertThat(payload.path("worker_id").asText()).isEqualTo(target.workerId().toString());
    assertThat(payload.path("worker_incarnation_id").asText())
        .isEqualTo(target.workerIncarnationId().toString());
    assertThat(payload.path("input").asText())
        .isEqualTo("Focus on the authorization failure first.");
    assertThat(payload.path("input_digest").asText())
        .isEqualTo("d4ab9135d40486358da5bba1d1a426cc2a76bd23c3c35f41fc07eda26f79983f");
  }

  @Test
  void steeringIdempotencyKeyCannotBeReusedForDifferentInput() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var repository = new JdbcRunRepository(jdbc, transactions);
    var run = repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(), "steering-conflict", "hello", RunStatus.QUEUED,
        1000, 100, 60, Instant.now()));
    markRunning(jdbc, transactions, ids, run.id());
    repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-1", "first", Instant.now());

    assertThatThrownBy(() -> repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-1", "different", Instant.now()))
        .isInstanceOf(RunSteeringConflict.class);
    assertThat(jdbc.queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.steering.requested'
        """, Integer.class, ids.tenantId(), run.id())).isOne();
  }

  @Test
  void steeringRateLimitUsesTwoSecondWindowAfterACompletedCommand() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var repository = new JdbcRunRepository(jdbc, transactions);
    var run = repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(), "steering-rate-limit", "hello", RunStatus.QUEUED,
        1000, 100, 60, Instant.now()));
    markRunning(jdbc, transactions, ids, run.id());
    var requestedAt = Instant.parse("2026-08-02T07:00:00Z");
    var first = repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-rate-1", "first", requestedAt);
    jdbc.update("""
        update run_steering_commands
           set state = 'rejected', rejection_reason = 'worker_rejected', rejected_at = ?,
               outcome_message_id = ?, updated_at = ?
         where tenant_id = ? and steering_id = ?
        """, Timestamp.from(requestedAt.plusMillis(500)), UUID.randomUUID(),
        Timestamp.from(requestedAt.plusMillis(500)), ids.tenantId(), first.steeringId());

    assertThatThrownBy(() -> repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-rate-2", "second",
        requestedAt.plusMillis(1_999)))
        .isInstanceOf(RuntimeException.class)
        .hasMessageContaining("2 second");

    var afterWindow = repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-rate-3", "third",
        requestedAt.plusSeconds(2));
    assertThat(afterWindow.state()).isEqualTo("pending");
    assertThat(jdbc.queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.steering.requested'
        """, Integer.class, ids.tenantId(), run.id())).isEqualTo(2);
  }

  @Test
  void steeringFailsClosedWhenTheRunIsNotAtARunningSafeBoundary() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var repository = new JdbcRunRepository(jdbc, transactions);
    var run = repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(), "steering-unsafe", "hello", RunStatus.QUEUED,
        1000, 100, 60, Instant.now()));
    markRunning(jdbc, transactions, ids, run.id());
    jdbc.update("""
        update runs set status = 'waiting_approval'
         where tenant_id = ? and id = ?
        """, ids.tenantId(), run.id());

    assertThatThrownBy(() -> repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-unsafe", "continue", Instant.now()))
        .isInstanceOf(RunSteeringNotAllowed.class);
    assertThat(jdbc.queryForObject("""
        select count(*) from run_steering_commands where tenant_id = ? and run_id = ?
        """, Integer.class, ids.tenantId(), run.id())).isZero();
  }

  @Test
  void cancellationClosesAPendingSteeringLedgerBeforeTheWorkerCanApplyIt() throws Exception {
    var ids = seedResourceChain();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var repository = new JdbcRunRepository(jdbc, transactions);
    var run = repository.save(ids.applicationId(), new Run(
        UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(), ids.workspaceId(),
        ids.modelPolicyId(), "steering-cancel", "hello", RunStatus.QUEUED,
        1000, 100, 60, Instant.now()));
    markRunning(jdbc, transactions, ids, run.id());
    var steering = repository.requestSteering(
        ids.tenantId(), ids.applicationId(), run.id(), "steer-before-cancel", "continue",
        Instant.now());

    assertThat(repository.requestCancellation(
        ids.tenantId(), ids.applicationId(), run.id(), Instant.now()))
        .isEqualTo(RunStatus.RUNNING);

    assertThat(jdbc.queryForObject("""
        select state from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, String.class, ids.tenantId(), steering.steeringId())).isEqualTo("cancelled");
  }

  private ResourceIds seedResourceChain() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    try (Connection connection = DriverManager.getConnection(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
        var statement = connection.createStatement()) {
      statement.execute("select set_config('app.tenant_id','%s',false)".formatted(tenantId));
      statement.execute("insert into tenants (tenant_id,id,slug,display_name) values ('%s','%s','t-%s','Tenant')"
          .formatted(tenantId, tenantId, tenantId));
      statement.execute("insert into applications (tenant_id,id,name) values ('%s','%s','App')"
          .formatted(tenantId, applicationId));
      statement.execute("insert into projects (tenant_id,id,application_id,name) values ('%s','%s','%s','Project')"
          .formatted(tenantId, projectId, applicationId));
      statement.execute("insert into workspaces (tenant_id,id,project_id,name) values ('%s','%s','%s','Workspace')"
          .formatted(tenantId, workspaceId, projectId));
      statement.execute("insert into agents (tenant_id,id,workspace_id,name) values ('%s','%s','%s','Agent')"
          .formatted(tenantId, agentId, workspaceId));
      statement.execute("insert into agent_versions (tenant_id,id,application_id,agent_id,version,spec) values ('%s','%s','%s','%s',1,'{}')"
          .formatted(tenantId, agentVersionId, applicationId, agentId));
      statement.execute("insert into sessions (tenant_id,id,workspace_id) values ('%s','%s','%s')"
          .formatted(tenantId, sessionId, workspaceId));
      statement.execute("insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id) values ('%s','%s','%s','Default','{}','%s')"
          .formatted(tenantId, modelPolicyId, workspaceId, applicationId));
    }
    return new ResourceIds(
        tenantId, applicationId, workspaceId, agentVersionId, sessionId, modelPolicyId);
  }

  private WorkerTarget markRunning(
      JdbcTemplate jdbc,
      TransactionTemplate transactions,
      ResourceIds ids,
      UUID runId) {
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var attemptId = UUID.randomUUID();
    transactions.executeWithoutResult(status -> {
      jdbc.queryForObject(
          "select set_config('app.tenant_id', ?, true)", String.class, ids.tenantId().toString());
      jdbc.update("""
          insert into runtime_workers (
            id,current_incarnation_id,placements,capacity,active_runs,runtime_version,last_heartbeat)
          values (?,?,array['cloud']::varchar[],1,1,'0.1.0',now())
          """, workerId, incarnationId);
      jdbc.update("""
          insert into runtime_worker_incarnations (
            worker_id,incarnation_id,placements,capacity,active_runs,runtime_version,last_heartbeat)
          values (?,?,array['cloud']::varchar[],1,1,'0.1.0',now())
          """, workerId, incarnationId);
      jdbc.update("""
          insert into run_dispatches (
            tenant_id,run_id,attempt_id,worker_id,worker_incarnation_id,owner_epoch,
            fencing_token,lease_expires_at,workload_identity_expires_at,
            state,requested_at,accepted_at)
          values (?,?,?,?,?,1,?,now() + interval '5 minutes',now() + interval '5 minutes',
                  'accepted',now(),now())
          """, ids.tenantId(), runId, attemptId, workerId, incarnationId, UUID.randomUUID());
      jdbc.update("""
          update runs set status = 'running', current_attempt_id = ?, updated_at = now()
           where tenant_id = ? and id = ?
          """, attemptId, ids.tenantId(), runId);
    });
    return new WorkerTarget(attemptId, workerId, incarnationId);
  }

  private ResourceIds seedApplicationResourceChain(UUID tenantId, String suffix) throws Exception {
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    try (Connection connection = DriverManager.getConnection(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
        var statement = connection.createStatement()) {
      statement.execute("select set_config('app.tenant_id','%s',false)".formatted(tenantId));
      statement.execute("insert into applications (tenant_id,id,name) values ('%s','%s','App %s')"
          .formatted(tenantId, applicationId, suffix));
      statement.execute("insert into projects (tenant_id,id,application_id,name) values ('%s','%s','%s','Project %s')"
          .formatted(tenantId, projectId, applicationId, suffix));
      statement.execute("insert into workspaces (tenant_id,id,project_id,name) values ('%s','%s','%s','Workspace %s')"
          .formatted(tenantId, workspaceId, projectId, suffix));
      statement.execute("insert into agents (tenant_id,id,workspace_id,name) values ('%s','%s','%s','Agent %s')"
          .formatted(tenantId, agentId, workspaceId, suffix));
      statement.execute("insert into agent_versions (tenant_id,id,application_id,agent_id,version,spec) values ('%s','%s','%s','%s',1,'{}')"
          .formatted(tenantId, agentVersionId, applicationId, agentId));
      statement.execute("insert into sessions (tenant_id,id,workspace_id) values ('%s','%s','%s')"
          .formatted(tenantId, sessionId, workspaceId));
      statement.execute("insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id) values ('%s','%s','%s','Default','{}','%s')"
          .formatted(tenantId, modelPolicyId, workspaceId, applicationId));
    }
    return new ResourceIds(
        tenantId, applicationId, workspaceId, agentVersionId, sessionId, modelPolicyId);
  }

  private void insertEvent(
      JdbcTemplate jdbc, ResourceIds ids, UUID runId, UUID eventId, long sequence, String type) {
    jdbc.queryForObject(
        "select set_config('app.tenant_id', ?, false)", String.class, ids.tenantId().toString());
    jdbc.update("""
        insert into run_events (
          tenant_id,event_id,run_id,session_id,sequence,schema_version,attempt_id,
          occurred_at,trace_id,type,payload,digest)
        values (?,?,?,?,?,1,?,now(),?,?,cast(? as jsonb),?)
        """, ids.tenantId(), eventId, runId, ids.sessionId(), sequence, UUID.randomUUID(),
        UUID.randomUUID().toString(), type, "{\"text\":\"ok\"}", "digest-" + sequence);
  }

  private record ResourceIds(
      UUID tenantId,
      UUID applicationId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID sessionId,
      UUID modelPolicyId) {}

  private record WorkerTarget(UUID attemptId, UUID workerId, UUID workerIncarnationId) {}
}
