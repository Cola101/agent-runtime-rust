package com.agentplatform.control.persistence;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.agentplatform.control.run.Run;
import com.agentplatform.control.run.RunStatus;
import com.agentplatform.control.run.SpawnSubagentCommand;
import com.agentplatform.control.run.SubagentAdmission;
import com.agentplatform.control.run.SubagentAdmissionRejected;
import com.agentplatform.control.run.SubagentAdmissionRejection;
import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.sql.Connection;
import java.sql.DriverManager;
import java.time.Instant;
import java.util.ArrayList;
import java.util.UUID;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.jdbc.datasource.DriverManagerDataSource;
import org.springframework.transaction.support.TransactionTemplate;

class JdbcSubagentAdmissionRepositoryIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("jdbc-subagent-admission");

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void repeatedDelegationCreatesOneChildAndReservesBudgetOnce() throws Exception {
    var fixture = fixture();
    var command = new SpawnSubagentCommand(
        UUID.randomUUID(), "reviewer", "Review the migration evidence.", 400, 30, 20);

    var first = fixture.admissions().admit(
        fixture.tenantId(), fixture.applicationId(), fixture.parentRunId(), command);
    var duplicate = fixture.admissions().admit(
        fixture.tenantId(), fixture.applicationId(), fixture.parentRunId(), command);

    assertThat(duplicate).isEqualTo(first);
    assertThat(first.rootRunId()).isEqualTo(fixture.parentRunId());
    assertThat(first.parentRunId()).isEqualTo(fixture.parentRunId());
    assertThat(first.delegationId()).isEqualTo(command.delegationId());
    assertThat(first.depth()).isOne();
    assertThat(first.role()).isEqualTo("reviewer");
    assertThat(first.remainingTokens()).isEqualTo(600);
    assertThat(first.remainingCostCents()).isEqualTo(70);
    assertThat(first.remainingDurationSeconds()).isEqualTo(40);
    assertThat(fixture.jdbc().queryForMap("""
        select root_run_id,parent_run_id,delegation_id,subagent_depth,agent_role,status
          from runs where tenant_id = ? and id = ?
        """, fixture.tenantId(), first.childRunId()))
        .containsEntry("root_run_id", fixture.parentRunId())
        .containsEntry("parent_run_id", fixture.parentRunId())
        .containsEntry("delegation_id", command.delegationId())
        .containsEntry("subagent_depth", 1)
        .containsEntry("agent_role", "reviewer")
        .containsEntry("status", "queued");
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from runs where tenant_id = ? and parent_run_id = ?
        """, Integer.class, fixture.tenantId(), fixture.parentRunId())).isOne();
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.queued'
        """, Integer.class, fixture.tenantId(), first.childRunId())).isOne();
    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.queued'
        """, String.class, fixture.tenantId(), first.childRunId()));
    assertThat(payload.path("budget").path("max_tokens").asLong()).isEqualTo(400);
  }

  @Test
  void delegationCannotBeReusedForDifferentIntent() throws Exception {
    var fixture = fixture();
    var delegationId = UUID.randomUUID();
    fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
        fixture.parentRunId(), new SpawnSubagentCommand(
            delegationId, "reviewer", "Review A.", 100, 10, 5));

    assertRejected(SubagentAdmissionRejection.DELEGATION_CONFLICT, () ->
        fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
            fixture.parentRunId(), new SpawnSubagentCommand(
                delegationId, "reviewer", "Review B.", 100, 10, 5)));
    assertThat(fixture.childCount(fixture.parentRunId())).isOne();
  }

  @Test
  void ninthActiveChildIsRejectedWithoutConsumingBudget() throws Exception {
    var fixture = fixture();
    for (int index = 0; index < 8; index++) {
      fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
          fixture.parentRunId(), command("Review " + index, 100, 10, 5));
    }

    assertRejected(SubagentAdmissionRejection.CHILD_CAPACITY, () ->
        fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
            fixture.parentRunId(), command("Review ninth", 100, 10, 5)));
    assertThat(fixture.childCount(fixture.parentRunId())).isEqualTo(8);
  }

  @Test
  void delegatedBudgetsCannotExceedTheParentsConservativeReservation() throws Exception {
    var fixture = fixture();
    fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
        fixture.parentRunId(), command("Large review", 600, 60, 30));

    assertRejected(SubagentAdmissionRejection.BUDGET_EXHAUSTED, () ->
        fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
            fixture.parentRunId(), command("Over budget", 500, 40, 30)));
    assertThat(fixture.childCount(fixture.parentRunId())).isOne();
  }

  @Test
  void aChildRoleCannotDelegateScopesItDidNotReceive() throws Exception {
    var fixture = fixture();
    var reader = fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
        fixture.parentRunId(), new SpawnSubagentCommand(
            UUID.randomUUID(), "reader", "Read metadata only.", 500, 50, 30));
    fixture.markRunning(reader.childRunId());

    assertRejected(SubagentAdmissionRejection.PERMISSION_ESCALATION, () ->
        fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
            reader.childRunId(), command("Escalating review", 100, 10, 5)));
    assertThat(fixture.childCount(reader.childRunId())).isZero();
  }

  @Test
  void depthThreeAgentCannotSpawnAnotherChild() throws Exception {
    var fixture = fixture();
    var parentId = fixture.parentRunId();
    long tokens = 900;
    long cost = 90;
    long duration = 50;
    SubagentAdmission child = null;
    for (int depth = 1; depth <= 3; depth++) {
      child = fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(), parentId,
          command("Depth " + depth, tokens, cost, duration));
      fixture.markRunning(child.childRunId());
      parentId = child.childRunId();
      tokens -= 100;
      cost -= 10;
      duration -= 10;
    }

    var depthThreeRunId = child.childRunId();
    assertRejected(SubagentAdmissionRejection.DEPTH_LIMIT, () ->
        fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
            depthThreeRunId, command("Depth four", 100, 10, 5)));
    assertThat(fixture.childCount(depthThreeRunId)).isZero();
  }

  @Test
  void concurrentAdmissionsCannotOversellOneParentBudget() throws Exception {
    var fixture = fixture();
    var start = new CountDownLatch(1);
    try (var executor = Executors.newFixedThreadPool(2)) {
      var futures = new ArrayList<java.util.concurrent.Future<Object>>();
      for (int index = 0; index < 2; index++) {
        var input = "Concurrent review " + index;
        futures.add(executor.submit(() -> {
          start.await();
          try {
            return fixture.admissions().admit(fixture.tenantId(), fixture.applicationId(),
                fixture.parentRunId(), command(input, 600, 60, 30));
          } catch (SubagentAdmissionRejected rejected) {
            return rejected.reason();
          }
        }));
      }
      start.countDown();
      var outcomes = new ArrayList<>();
      for (var future : futures) {
        outcomes.add(future.get());
      }
      assertThat(outcomes.stream().filter(SubagentAdmission.class::isInstance)).hasSize(1);
      assertThat(outcomes).contains(SubagentAdmissionRejection.BUDGET_EXHAUSTED);
    }
    assertThat(fixture.childCount(fixture.parentRunId())).isOne();
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events o
          join runs r on r.tenant_id = o.tenant_id and r.id = o.aggregate_id
         where o.tenant_id = ? and o.event_type = 'run.queued'
           and r.parent_run_id = ?
        """, Integer.class, fixture.tenantId(), fixture.parentRunId())).isOne();
  }

  private SpawnSubagentCommand command(
      String input, long maxTokens, long maxCostCents, long maxDurationSeconds) {
    return new SpawnSubagentCommand(
        UUID.randomUUID(), "reviewer", input, maxTokens, maxCostCents, maxDurationSeconds);
  }

  private void assertRejected(SubagentAdmissionRejection reason, Runnable admission) {
    assertThatThrownBy(admission::run)
        .isInstanceOf(SubagentAdmissionRejected.class)
        .satisfies(error -> assertThat(((SubagentAdmissionRejected) error).reason())
            .isEqualTo(reason));
  }

  private Fixture fixture() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    var parentRunId = UUID.randomUUID();
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
      statement.execute("""
          insert into agent_versions (tenant_id,id,application_id,agent_id,version,spec)
          values ('%s','%s','%s','%s',1,
            '{"instructions":"Coordinate reviews.",
              "delegated_scopes":["tool:workspace.read"],
              "subagent_roles":[{"name":"reviewer",
                "instructions":"Review evidence only.",
                "delegated_scopes":["tool:workspace.read"]},
                {"name":"reader","instructions":"Read metadata only.",
                "delegated_scopes":[]}]}')
          """.formatted(tenantId, agentVersionId, applicationId, agentId));
      statement.execute("insert into sessions (tenant_id,id,workspace_id) values ('%s','%s','%s')"
          .formatted(tenantId, sessionId, workspaceId));
      statement.execute("insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id) values ('%s','%s','%s','Default','{}','%s')"
          .formatted(tenantId, modelPolicyId, workspaceId, applicationId));
    }
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var runs = new JdbcRunRepository(jdbc, transactions);
    runs.save(applicationId, new Run(
        parentRunId, tenantId, sessionId, agentVersionId, workspaceId, modelPolicyId,
        "subagent-parent", "Coordinate the review.", RunStatus.QUEUED,
        1000, 100, 60, Instant.now()));
    jdbc.update("update runs set status = 'running' where tenant_id = ? and id = ?",
        tenantId, parentRunId);
    return new Fixture(
        tenantId, applicationId, parentRunId, jdbc,
        new JdbcSubagentAdmissionRepository(jdbc, transactions));
  }

  private record Fixture(
      UUID tenantId,
      UUID applicationId,
      UUID parentRunId,
      JdbcTemplate jdbc,
      JdbcSubagentAdmissionRepository admissions) {

    void markRunning(UUID runId) {
      jdbc.update("update runs set status = 'running' where tenant_id = ? and id = ?",
          tenantId, runId);
    }

    int childCount(UUID runId) {
      return jdbc.queryForObject("""
          select count(*) from runs where tenant_id = ? and parent_run_id = ?
          """, Integer.class, tenantId, runId);
    }
  }
}
