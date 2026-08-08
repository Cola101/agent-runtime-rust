package com.agentplatform.control.scheduler;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.agentplatform.control.approval.ApprovalConflict;
import com.agentplatform.control.approval.ApprovalDecision;
import com.agentplatform.control.approval.ApprovalDecisionNotAllowed;
import com.agentplatform.control.approval.ApprovalNotFound;
import com.agentplatform.control.approval.DecideApprovalCommand;
import com.agentplatform.control.approval.JdbcApprovalRepository;
import com.agentplatform.control.persistence.JdbcRunRepository;
import com.agentplatform.control.identity.Ed25519WorkloadTokenIssuer;
import com.agentplatform.control.run.Run;
import com.agentplatform.control.run.RunStatus;
import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.sql.Connection;
import java.sql.DriverManager;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.KeyPairGenerator;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.Map;
import java.util.Base64;
import java.util.HexFormat;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.jdbc.datasource.DriverManagerDataSource;
import org.springframework.transaction.support.TransactionTemplate;
import com.fasterxml.jackson.databind.ObjectMapper;

class JdbcSchedulerRepositoryIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("jdbc-scheduler-repository");

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void dispatchDelegatesOnlyTheScopesDeclaredByTheImmutableAgentVersion() throws Exception {
    var fixture = fixture("scheduler-delegated-scopes");
    fixture.jdbc().update("""
        update agent_versions
           set spec = '{"instructions":"Inspect evidence before conclusions.",
                        "delegated_scopes":["workspace:read","tool:http"]}'::jsonb
         where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.agentVersionId());
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();

    assertThat(command.delegatedScopes()).containsExactly("tool:http", "workspace:read");
    assertThat(command.schemaVersion()).isEqualTo(9);
    // A root dispatch carries whatever the AgentVersion configured; a subagent
    // dispatch carries nothing, because a role-scoped exemption is a second
    // decision nobody has made.
    if (command.lineage().depth() > 0) {
      assertThat(command.toolApprovalPolicies()).isEmpty();
    }
    assertThat(command.agentInstructions()).isEqualTo("Inspect evidence before conclusions.");
    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("delegated_scopes").toString())
        .isEqualTo("[\"tool:http\",\"workspace:read\"]");
    assertThat(payload.path("agent_instructions").asText())
        .isEqualTo("Inspect evidence before conclusions.");
  }

  @Test
  void steeringReceiptMustMatchThePendingLedgerBeforeItAdvancesTheRunSequence() throws Exception {
    var fixture = fixture("scheduler-steering-receipt");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()))).isTrue();
    var started = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", started,
        sha256(started)))).isTrue();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var runs = new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var steering = runs.requestSteering(
        fixture.tenantId(), fixture.applicationId(), fixture.runId(), "steer-receipt",
        "Focus on authorization.", Instant.now());
    var inputDigest = sha256("Focus on authorization.");
    var applied = """
        {"status":"running","steering_id":"%s","input_digest":"%s"}
        """.formatted(steering.steeringId(), inputDigest).strip();

    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.steer.applied", applied,
        sha256(applied)))).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select state,applied_event_id from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, fixture.tenantId(), steering.steeringId()))
        .containsEntry("state", "applied")
        .doesNotContainEntry("applied_event_id", null);
    assertThat(fixture.jdbc().queryForObject("""
        select last_sequence from runs where tenant_id = ? and id = ?
        """, Long.class, fixture.tenantId(), fixture.runId())).isEqualTo(2L);

    var forged = """
        {"status":"running","steering_id":"%s","input_digest":"%s"}
        """.formatted(UUID.randomUUID(), inputDigest).strip();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 3,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.steer.applied", forged,
        sha256(forged)))).isFalse();
    assertThat(fixture.jdbc().queryForObject("""
        select last_sequence from runs where tenant_id = ? and id = ?
        """, Long.class, fixture.tenantId(), fixture.runId())).isEqualTo(2L);
  }

  @Test
  void terminalRunEventRejectsAnySteeringCommandThatDidNotReachAnAppliedBoundary()
      throws Exception {
    var fixture = fixture("scheduler-steering-terminal-race");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var started = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", started,
        sha256(started)))).isTrue();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var runs = new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var steering = runs.requestSteering(
        fixture.tenantId(), fixture.applicationId(), fixture.runId(), "steer-terminal-race",
        "This command loses the terminal race.", Instant.now());
    var succeeded = "{\"status\":\"succeeded\",\"reason\":\"stop\"}";

    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.succeeded", succeeded,
        sha256(succeeded)))).isTrue();

    assertThat(fixture.jdbc().queryForObject("""
        select state from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, String.class, fixture.tenantId(), steering.steeringId())).isEqualTo("rejected");
  }

  @Test
  void exactSteeringRejectionClosesTheLedgerWhileAForgedOutcomeCannotMutateIt()
      throws Exception {
    var fixture = fixture("scheduler-steering-rejection");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), command.workerIncarnationId(), Instant.now()));
    var started = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", started,
        sha256(started)))).isTrue();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var runs = new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var steering = runs.requestSteering(
        fixture.tenantId(), fixture.applicationId(), fixture.runId(), "steer-rejection",
        "Focus on the durable ledger.", Instant.now());
    var inputDigest = sha256("Focus on the durable ledger.");
    var forged = new RunSteeringOutcomeMessage(
        1, UUID.randomUUID(), steering.steeringId(), fixture.tenantId(), fixture.runId(),
        UUID.randomUUID(), command.workerId(), command.workerIncarnationId(), inputDigest,
        "rejected", "expired", Instant.now());

    assertThat(fixture.scheduler().recordSteeringOutcome(forged)).isFalse();
    assertThat(fixture.jdbc().queryForObject("""
        select state from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, String.class, fixture.tenantId(), steering.steeringId())).isEqualTo("pending");

    var messageId = UUID.randomUUID();
    var rejectedAt = Instant.now();
    var exact = new RunSteeringOutcomeMessage(
        1, messageId, steering.steeringId(), fixture.tenantId(), fixture.runId(),
        command.attemptId(), command.workerId(), command.workerIncarnationId(), inputDigest,
        "rejected", "expired", rejectedAt);
    assertThat(fixture.scheduler().recordSteeringOutcome(exact)).isTrue();
    assertThat(fixture.scheduler().recordSteeringOutcome(exact)).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select state,rejection_reason,outcome_message_id,rejected_at
          from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, fixture.tenantId(), steering.steeringId()))
        .containsEntry("state", "rejected")
        .containsEntry("rejection_reason", "expired")
        .containsEntry("outcome_message_id", messageId);
  }

  @Test
  void dispatchCarriesOnlySubagentRolesWithinTheCurrentRunAuthority() throws Exception {
    var fixture = fixture("scheduler-subagent-role-catalog");
    fixture.jdbc().update("""
        update agent_versions
           set spec = '{"instructions":"Coordinate bounded reviews.",
                        "delegated_scopes":["agent:spawn","tool:workspace.read"],
                        "subagent_roles":[
                          {"name":"reviewer","instructions":"Review evidence only.",
                           "delegated_scopes":["tool:workspace.read"]},
                          {"name":"operator","instructions":"Call external systems.",
                           "delegated_scopes":["tool:http"]}]}'::jsonb
         where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.agentVersionId());
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();

    assertThat(command.schemaVersion()).isEqualTo(9);
    assertThat(command.subagentRoles()).containsExactly(new SubagentRoleSnapshot(
        "reviewer", "Review evidence only.", List.of("tool:workspace.read")));
    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("subagent_roles").toString()).isEqualTo(
        "[{\"name\":\"reviewer\",\"instructions\":\"Review evidence only.\"," +
            "\"delegated_scopes\":[\"tool:workspace.read\"]}]");
  }

  @Test
  void dispatchCarriesOrderedSignedSkillSnapshotsBoundToTheAgentVersion() throws Exception {
    var fixture = fixture("scheduler-skill-snapshot");
    var skillId = UUID.randomUUID();
    var skillVersionId = UUID.randomUUID();
    var digest = "a".repeat(64);
    var signature = "A".repeat(86);
    fixture.jdbc().update("""
        insert into skills (tenant_id,id,application_id,name)
        values (?,?,?,'workspace-review')
        """, fixture.tenantId(), skillId, fixture.applicationId());
    fixture.jdbc().update("""
        insert into skill_versions (
          tenant_id,id,application_id,skill_id,semantic_version,artifact,
          artifact_digest,signing_key_id,signature)
        values (?,?,?,?, '1.0.0', jsonb_build_object(
          'schema_version',1,
          'tenant_id',?::text,
          'application_id',?::text,
          'skill_version_id',?::text,
          'name','workspace-review',
          'semantic_version','1.0.0',
          'description','Review bounded workspace evidence',
          'instructions','Inspect evidence before conclusions.',
          'tool_names',jsonb_build_array('workspace.read_text'),
          'supported_platforms',jsonb_build_array('darwin-arm64'),
          'min_runtime_version','0.1.0'), ?, 'skill-key-v1', ?)
        """, fixture.tenantId(), skillVersionId, fixture.applicationId(), skillId,
        fixture.tenantId().toString(), fixture.applicationId().toString(),
        skillVersionId.toString(), digest, signature);
    fixture.jdbc().update("""
        insert into agent_version_skills (
          tenant_id,application_id,agent_version_id,ordinal,skill_version_id,artifact_digest)
        values (?,?,?,?,?,?)
        """, fixture.tenantId(), fixture.applicationId(), fixture.agentVersionId(), 0,
        skillVersionId, digest);
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();

    assertThat(command.schemaVersion()).isEqualTo(9);
    assertThat(command.skillSnapshots()).hasSize(1);
    assertThat(command.skillSnapshots().getFirst().skillVersionId()).isEqualTo(skillVersionId);
    assertThat(command.skillSnapshots().getFirst().toolNames())
        .containsExactly("workspace.read_text");
    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("skill_snapshots").get(0).path("artifact_digest").asText())
        .isEqualTo(digest);
    assertThat(payload.path("skill_snapshots").get(0).path("signature").asText())
        .isEqualTo(signature);
  }

  @Test
  void rootDispatchCarriesDatabaseBackedLineageInTheCommandAndOutbox() throws Exception {
    var fixture = fixture("scheduler-root-lineage");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();

    assertThat(command.schemaVersion()).isEqualTo(9);
    assertThat(command.lineage().rootRunId()).isEqualTo(fixture.runId());
    assertThat(command.lineage().parentRunId()).isNull();
    assertThat(command.lineage().delegationId()).isNull();
    assertThat(command.lineage().depth()).isZero();
    assertThat(command.lineage().role()).isEqualTo("primary");
    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("lineage").path("root_run_id").asText())
        .isEqualTo(fixture.runId().toString());
    assertThat(payload.path("lineage").path("role").asText()).isEqualTo("primary");
  }

  @Test
  void childDispatchUsesOnlyItsImmutableRoleInstructionsAndScopeSubset() throws Exception {
    var fixture = fixture("scheduler-child-role");
    var parentRunId = UUID.randomUUID();
    var delegationId = UUID.randomUUID();
    fixture.jdbc().update("""
        update agent_versions set spec = jsonb_build_object(
          'instructions','Coordinate the review.',
          'delegated_scopes',jsonb_build_array('tool:http','tool:workspace.read'),
          'subagent_roles',jsonb_build_array(jsonb_build_object(
            'name','reviewer',
            'instructions','Read evidence and report findings only.',
            'delegated_scopes',jsonb_build_array('tool:workspace.read'))))
         where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.agentVersionId());
    fixture.jdbc().update("""
        insert into runs (
          tenant_id,application_id,id,session_id,workspace_id,agent_version_id,model_policy_id,
          idempotency_key,input,status,max_tokens,max_cost_cents,max_duration_seconds)
        select tenant_id,application_id,?,session_id,workspace_id,agent_version_id,model_policy_id,
               'scheduler-child-parent','parent','running',max_tokens,max_cost_cents,
               max_duration_seconds
          from runs where tenant_id = ? and id = ?
        """, parentRunId, fixture.tenantId(), fixture.runId());
    fixture.jdbc().update("""
        update runs
           set root_run_id = ?, parent_run_id = ?, delegation_id = ?,
               subagent_depth = 1, agent_role = 'reviewer'
         where tenant_id = ? and id = ?
        """, parentRunId, parentRunId, delegationId, fixture.tenantId(), fixture.runId());
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();

    assertThat(command.lineage()).isEqualTo(new AgentLineageSnapshot(
        parentRunId, parentRunId, delegationId, 1, "reviewer"));
    assertThat(command.delegatedScopes()).containsExactly("tool:workspace.read");
    assertThat(command.agentInstructions()).isEqualTo("""
        Coordinate the review.

        [Subagent role reviewer]
        Read evidence and report findings only.""");
  }

  @Test
  void healthyWorkerGetsOneFencedDispatchAndDuplicateDeliveryIsIdempotent() throws Exception {
    var fixture = fixture("scheduler-dispatch");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 1, "0.1.0"));

    var first = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));
    var duplicate = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));

    assertThat(first.status()).isEqualTo(ScheduleStatus.DISPATCHED);
    assertThat(first.command()).isPresent();
    assertThat(first.command().orElseThrow().workerId()).isEqualTo(workerId);
    assertThat(first.command().orElseThrow().ownerEpoch()).isOne();
    assertThat(first.command().orElseThrow().fencingToken()).isNotNull();
    assertThat(first.command().orElseThrow().modelPolicyId()).isEqualTo(fixture.modelPolicyId());
    assertThat(first.command().orElseThrow().workloadToken().toString())
        .isEqualTo("WorkloadToken[REDACTED]");
    assertThat(duplicate.status()).isEqualTo(ScheduleStatus.ALREADY_DISPATCHED);
    assertThat(duplicate.command()).contains(first.command().orElseThrow());

    assertThat(fixture.jdbc().queryForObject(
        "select status from runs where tenant_id = ? and id = ?",
        String.class, fixture.tenantId(), fixture.runId())).isEqualTo("queued");
    assertThat(fixture.jdbc().queryForObject(
        "select count(*) from run_dispatches where tenant_id = ? and run_id = ?",
        Integer.class, fixture.tenantId(), fixture.runId())).isOne();
    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("model_policy_id").asText()).isEqualTo(fixture.modelPolicyId().toString());
    assertThat(payload.path("workload_token").asText()).isEqualTo(
        first.command().orElseThrow().workloadToken().value());
    var tokenParts = payload.path("workload_token").asText().split("\\.");
    var claims = new ObjectMapper().readTree(Base64.getUrlDecoder().decode(tokenParts[1]));
    assertThat(claims.path("tenant_id").asText()).isEqualTo(fixture.tenantId().toString());
    assertThat(claims.path("run_id").asText()).isEqualTo(fixture.runId().toString());
    assertThat(claims.path("attempt_id").asText())
        .isEqualTo(first.command().orElseThrow().attemptId().toString());
    assertThat(claims.path("worker_id").asText()).isEqualTo(workerId.toString());
    assertThat(claims.path("worker_incarnation_id").asText())
        .isEqualTo(first.command().orElseThrow().workerIncarnationId().toString());
    assertThat(claims.path("schema_version").asInt()).isEqualTo(2);
    assertThat(claims.path("model_policy_id").asText()).isEqualTo(fixture.modelPolicyId().toString());
    assertThat(fixture.jdbc().queryForObject(
        "select count(*) from outbox_events where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'",
        Integer.class, fixture.tenantId(), fixture.runId())).isOne();
  }

  @Test
  void orderedProviderSnapshotIsDigestBoundToTheExecutionAndWorkloadIdentity() throws Exception {
    var fixture = fixture("scheduler-provider-snapshot");
    var primary = UUID.randomUUID();
    var fallback = UUID.randomUUID();
    var envelope = """
        {"schema_version":1,"key_id":"gateway-key","algorithm":"RSA-OAEP-256+A256GCM",
         "encrypted_key":"wrapped","nonce":"nonce","ciphertext":"ciphertext"}
        """;
    fixture.jdbc().update("""
        insert into model_providers (
          tenant_id,id,application_id,name,protocol,endpoint,model,credential_envelope)
        values (?,?,?,'Primary','openai_responses','https://primary.example.test/v1/responses',
                'primary-model',?::jsonb),
               (?,?,?,'Fallback','anthropic_messages','https://fallback.example.test/v1/messages',
                'fallback-model',?::jsonb)
        """, fixture.tenantId(), primary, fixture.applicationId(), envelope,
        fixture.tenantId(), fallback, fixture.applicationId(), envelope);
    fixture.jdbc().update("""
        update model_policies set policy = '{"routing":"ordered_failover"}'::jsonb
         where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.modelPolicyId());
    fixture.jdbc().update("""
        insert into model_policy_candidates (
          tenant_id,application_id,model_policy_id,provider_id,priority)
        values (?,?,?,?,0),(?,?,?,?,1)
        """, fixture.tenantId(), fixture.applicationId(), fixture.modelPolicyId(), primary,
        fixture.tenantId(), fixture.applicationId(), fixture.modelPolicyId(), fallback);
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 2, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    var snapshotBytes = Base64.getDecoder().decode(command.modelPolicySnapshotBase64());
    var snapshot = new ObjectMapper().readTree(snapshotBytes);
    var expectedDigest = HexFormat.of().formatHex(
        MessageDigest.getInstance("SHA-256").digest(snapshotBytes));
    var tokenParts = command.workloadToken().value().split("\\.");
    var claims = new ObjectMapper().readTree(Base64.getUrlDecoder().decode(tokenParts[1]));

    assertThat(command.schemaVersion()).isEqualTo(9);
    assertThat(snapshot.path("routing").asText()).isEqualTo("ordered_failover");
    assertThat(snapshot.path("candidates").findValuesAsText("provider_id"))
        .containsExactly(primary.toString(), fallback.toString());
    assertThat(snapshot.path("candidates").get(0).path("credential_envelope").isObject()).isTrue();
    assertThat(command.modelPolicyDigest()).isEqualTo(expectedDigest);
    assertThat(claims.path("schema_version").asInt()).isEqualTo(3);
    assertThat(claims.path("model_policy_digest").asText()).isEqualTo(expectedDigest);
    var outbox = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.execution.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(outbox.path("model_policy_snapshot_base64").asText())
        .isEqualTo(command.modelPolicySnapshotBase64());
    assertThat(outbox.path("model_policy_digest").asText()).isEqualTo(expectedDigest);
    assertThat(outbox.toString()).doesNotContain("tenant-api-key");
  }

  @Test
  void noHealthyCapacityLeavesRunQueued() throws Exception {
    var fixture = fixture("scheduler-no-capacity");
    fixture.jdbc().update("update runtime_workers set last_heartbeat = now() - interval '1 hour'");
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), UUID.randomUUID(), Instant.now(), List.of("cloud"), 2, 2, "0.1.0"));

    var result = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));

    assertThat(result.status()).isEqualTo(ScheduleStatus.RETRY_NO_CAPACITY);
    assertThat(result.command()).isEmpty();
    assertThat(fixture.jdbc().queryForObject(
        "select status from runs where tenant_id = ? and id = ?",
        String.class, fixture.tenantId(), fixture.runId())).isEqualTo("queued");
  }

  @Test
  void workerReportedClockSkewCannotExtendSchedulerFreshness() throws Exception {
    var fixture = fixture("scheduler-worker-clock-skew");
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, incarnationId, Instant.now().plus(Duration.ofDays(1)),
        List.of("cloud"), 2, 0, List.of(), "0.1.0"), Duration.ofSeconds(30));
    fixture.jdbc().update("""
        update runtime_worker_incarnations
           set last_heartbeat_received_at = now() - interval '1 minute'
         where worker_id = ? and incarnation_id = ?
        """, workerId, incarnationId);

    var result = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));

    assertThat(result.status()).isEqualTo(ScheduleStatus.RETRY_NO_CAPACITY);
    assertThat(result.command()).isEmpty();
  }

  @Test
  void explicitTerminalEventClosesARecoveryIncidentEvenBeforeRunRestored() throws Exception {
    var fixture = fixture("scheduler-recovery-terminal");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()))).isTrue();
    fixture.jdbc().update("""
        insert into recovery_incidents (
          tenant_id,incident_id,run_id,failed_attempt_id,failed_worker_id,
          failed_worker_incarnation_id,recovery_attempt_id,last_confirmed_healthy_at,state)
        values (?,?,?,?,?,?,?,now() - interval '1 second','recovery_requested')
        """, fixture.tenantId(), UUID.randomUUID(), fixture.runId(), command.attemptId(),
        command.workerId(), command.workerIncarnationId(), command.attemptId());
    var terminalPayload = "{\"error\":\"injected recovery failure\"}";

    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.failed", terminalPayload,
        sha256(terminalPayload)))).isTrue();

    assertThat(fixture.jdbc().queryForMap("""
        select state,resolved_at from recovery_incidents where tenant_id = ? and run_id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("state", "terminated")
        .doesNotContainEntry("resolved_at", null);
    assertThat(fixture.scheduler().recoverySloSnapshot(
        fixture.tenantId(), Duration.ofMinutes(15)))
        .isEqualTo(new RecoverySloSnapshot(0, 0, 0, 0, 0));
  }

  @Test
  void durableSubagentCheckpointAtomicallyQueuesChildAndReleasesParentWorkspaceLease()
      throws Exception {
    var fixture = fixture("scheduler-subagent-handoff");
    fixture.jdbc().update("""
        update agent_versions
           set spec = ?::jsonb
         where tenant_id = ? and id = ?
        """, """
        {"delegated_scopes":["agent:spawn","tool:workspace.read"],
         "subagent_roles":[{"name":"reviewer","instructions":"Review evidence only.",
          "delegated_scopes":["tool:workspace.read"]}]}
        """, fixture.tenantId(), fixture.agentVersionId());
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 2, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()))).isTrue();
    var startedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", startedPayload,
        sha256(startedPayload)))).isTrue();

    var delegationId = UUID.randomUUID();
    var bindingDigest = "b".repeat(64);
    var spawnPayload = """
        {"status":"suspended","request":{"tool_call_id":"call-review",
         "delegation_id":"%s","role":"reviewer","input":"Review migration evidence.",
         "budget":{"max_tokens":400,"max_cost_cents":30,"max_duration_seconds":20},
         "binding_digest":"%s"}}
        """.formatted(delegationId, bindingDigest).strip();
    var spawnEvent = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "subagent.spawn.requested",
        spawnPayload, sha256(spawnPayload));
    assertThat(fixture.scheduler().recordRunEvent(spawnEvent)).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select status,last_sequence from runs where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("status", "running")
        .containsEntry("last_sequence", 2L);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from runs where tenant_id = ? and parent_run_id = ?
        """, Integer.class, fixture.tenantId(), fixture.runId())).isZero();

    var checkpointPayload = "{\"pending_subagent\":{\"binding_digest\":\"%s\"}}"
        .formatted(bindingDigest).getBytes(StandardCharsets.UTF_8);
    var checkpoint = new RunCheckpointMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.sessionId(),
        command.attemptId(), command.ownerEpoch(), command.fencingToken(), 2, "suspended",
        "a".repeat(64), "c".repeat(64), checkpointPayload,
        sha256(new String(checkpointPayload, StandardCharsets.UTF_8)), Instant.now());
    assertThat(fixture.scheduler().recordCheckpoint(checkpoint)).isTrue();
    assertThat(fixture.scheduler().recordCheckpoint(checkpoint)).isTrue();

    assertThat(fixture.jdbc().queryForMap("""
        select status,current_attempt_id from runs where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("status", "suspended")
        .containsEntry("current_attempt_id", command.attemptId());
    assertThat(fixture.jdbc().queryForMap("""
        select state from run_dispatches
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, fixture.tenantId(), fixture.runId(), command.attemptId()))
        .containsEntry("state", "suspended");
    assertThat(fixture.jdbc().queryForMap("""
        select id,status,parent_run_id,delegation_id,agent_role
          from runs where tenant_id = ? and parent_run_id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("status", "queued")
        .containsEntry("parent_run_id", fixture.runId())
        .containsEntry("delegation_id", delegationId)
        .containsEntry("agent_role", "reviewer");
    assertThat(fixture.jdbc().queryForObject("""
        select active_runs from runtime_workers where id = ?
        """, Integer.class, command.workerId())).isZero();
    assertThat(fixture.jdbc().queryForObject("""
        select expires_at <= clock_timestamp() from workspace_leases
         where tenant_id = ? and workspace_id = ? and owner_epoch = ?
        """, Boolean.class, fixture.tenantId(), fixture.workspaceId(), command.ownerEpoch()))
        .isTrue();

    var childRunId = fixture.jdbc().queryForObject("""
        select child_run_id from subagent_calls
         where tenant_id = ? and parent_run_id = ? and tool_call_id = 'call-review'
        """, UUID.class, fixture.tenantId(), fixture.runId());
    var child = fixture.scheduler().schedule(
        fixture.tenantId(), childRunId, Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), child.tenantId(), child.runId(), child.attemptId(),
        child.workerId(), Instant.now()))).isTrue();
    var childStarted = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, child.tenantId(), child.sessionId(), child.runId(), 1,
        child.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", childStarted,
        sha256(childStarted)))).isTrue();
    var childDelta = "{\"text\":\"Migration evidence is consistent.\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, child.tenantId(), child.sessionId(), child.runId(), 2,
        child.attemptId(), Instant.now(), UUID.randomUUID(), "model.output.delta", childDelta,
        sha256(childDelta)))).isTrue();
    var childTerminalEventId = UUID.randomUUID();
    var childSucceeded = "{\"status\":\"succeeded\",\"reason\":\"stop\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        childTerminalEventId, 1, child.tenantId(), child.sessionId(), child.runId(), 3,
        child.attemptId(), Instant.now(), UUID.randomUUID(), "run.succeeded", childSucceeded,
        sha256(childSucceeded)))).isTrue();

    assertThat(fixture.jdbc().queryForMap("""
        select state,child_terminal_event_id,result->>'text' as result_text
          from subagent_calls
         where tenant_id = ? and parent_run_id = ? and tool_call_id = 'call-review'
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("state", "result_ready")
        .containsEntry("child_terminal_event_id", childTerminalEventId)
        .containsEntry("result_text", "Migration evidence is consistent.");

    assertThat(fixture.scheduler().reconcileExpired())
        .isEqualTo(new ReconcileResult(0, 1, 0));
    var recoveryPayload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.recovery.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(recoveryPayload.path("schema_version").asInt()).isEqualTo(2);
    assertThat(recoveryPayload.path("subagent_result").path("tool_call_id").asText())
        .isEqualTo("call-review");
    assertThat(recoveryPayload.path("subagent_result").path("child_run_id").asText())
        .isEqualTo(childRunId.toString());
    assertThat(recoveryPayload.path("subagent_result").path("child_terminal_event_id").asText())
        .isEqualTo(childTerminalEventId.toString());
    assertThat(recoveryPayload.path("subagent_result").path("content").path("text").asText())
        .isEqualTo("Migration evidence is consistent.");
    assertThat(fixture.scheduler().reconcileExpired())
        .isEqualTo(new ReconcileResult(0, 0, 0));

    var recoveryAttemptId = UUID.fromString(
        recoveryPayload.path("execution").path("attempt_id").asText());
    var recoveryWorkerId = UUID.fromString(
        recoveryPayload.path("execution").path("worker_id").asText());
    var recoveryWorkerIncarnationId = UUID.fromString(
        recoveryPayload.path("execution").path("worker_incarnation_id").asText());
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        2, UUID.randomUUID(), fixture.tenantId(), fixture.runId(), recoveryAttemptId,
        recoveryWorkerId, recoveryWorkerIncarnationId, Instant.now()))).isTrue();
    var restoredPayload = "{\"status\":\"suspended\",\"checkpoint_digest\":\"%s\"}"
        .formatted("a".repeat(64));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, fixture.tenantId(), command.sessionId(), fixture.runId(), 3,
        recoveryAttemptId, Instant.now(), UUID.randomUUID(), "run.restored", restoredPayload,
        sha256(restoredPayload)))).isTrue();
    var resultReceived = new ObjectMapper().writeValueAsString(Map.of(
        "status", "running",
        "tool_call_id", "call-review",
        "delegation_id", delegationId,
        "binding_digest", bindingDigest,
        "child_run_id", childRunId,
        "child_terminal_event_id", childTerminalEventId,
        "terminal_status", "succeeded",
        "is_error", false,
        "result_digest", recoveryPayload.path("subagent_result").path("digest").asText()));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, fixture.tenantId(), command.sessionId(), fixture.runId(), 4,
        recoveryAttemptId, Instant.now(), UUID.randomUUID(), "subagent.result.received",
        resultReceived, sha256(resultReceived)))).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select r.status,s.state,s.delivered_event_id
          from runs r join subagent_calls s
            on s.tenant_id = r.tenant_id and s.parent_run_id = r.id
         where r.tenant_id = ? and r.id = ? and s.tool_call_id = 'call-review'
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("status", "running")
        .containsEntry("state", "delivered")
        .doesNotContainEntry("delivered_event_id", null);
  }

  @Test
  void cancellingSuspendedParentTargetsEveryActiveRunInItsSubagentTree() throws Exception {
    var fixture = fixture("scheduler-subagent-cancellation");
    fixture.jdbc().update("""
        update agent_versions
           set spec = ?::jsonb
         where tenant_id = ? and id = ?
        """, """
        {"delegated_scopes":["agent:spawn","tool:workspace.read"],
         "subagent_roles":[{"name":"reviewer","instructions":"Review evidence only.",
          "delegated_scopes":["tool:workspace.read"]}]}
        """, fixture.tenantId(), fixture.agentVersionId());
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 2, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var parent = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), parent.tenantId(), parent.runId(), parent.attemptId(),
        parent.workerId(), Instant.now()))).isTrue();
    var delegationId = UUID.randomUUID();
    var bindingDigest = "d".repeat(64);
    var spawnPayload = """
        {"status":"suspended","request":{"tool_call_id":"call-cancel",
         "delegation_id":"%s","role":"reviewer","input":"Review cancellation.",
         "budget":{"max_tokens":400,"max_cost_cents":30,"max_duration_seconds":20},
         "binding_digest":"%s"}}
        """.formatted(delegationId, bindingDigest).strip();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, parent.tenantId(), parent.sessionId(), parent.runId(), 1,
        parent.attemptId(), Instant.now(), UUID.randomUUID(), "subagent.spawn.requested",
        spawnPayload, sha256(spawnPayload)))).isTrue();
    var checkpointPayload = "{\"pending_subagent\":{\"binding_digest\":\"%s\"}}"
        .formatted(bindingDigest).getBytes(StandardCharsets.UTF_8);
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), parent.tenantId(), parent.runId(), parent.sessionId(),
        parent.attemptId(), parent.ownerEpoch(), parent.fencingToken(), 1, "suspended",
        "a".repeat(64), "c".repeat(64), checkpointPayload,
        sha256(new String(checkpointPayload, StandardCharsets.UTF_8)), Instant.now()))).isTrue();
    var childRunId = fixture.jdbc().queryForObject("""
        select child_run_id from subagent_calls
         where tenant_id = ? and parent_run_id = ? and tool_call_id = 'call-cancel'
        """, UUID.class, fixture.tenantId(), fixture.runId());
    var child = fixture.scheduler().schedule(
        fixture.tenantId(), childRunId, Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), child.tenantId(), child.runId(), child.attemptId(),
        child.workerId(), Instant.now()))).isTrue();
    var runs = new JdbcRunRepository(
        fixture.jdbc(), new TransactionTemplate(
            new DataSourceTransactionManager(fixture.jdbc().getDataSource())));

    assertThat(runs.requestCancellation(
        fixture.tenantId(), fixture.runId(), Instant.now()))
        .isEqualTo(RunStatus.SUSPENDED);

    assertThat(fixture.jdbc().queryForList("""
        select aggregate_id from outbox_events
         where tenant_id = ? and event_type = 'run.cancellation.requested'
           and aggregate_id in (?,?) order by aggregate_id
        """, UUID.class, fixture.tenantId(), fixture.runId(), childRunId))
        .containsExactlyInAnyOrder(fixture.runId(), childRunId);
    assertThat(fixture.jdbc().queryForObject("""
        select state from subagent_calls
         where tenant_id = ? and parent_run_id = ? and tool_call_id = 'call-cancel'
        """, String.class, fixture.tenantId(), fixture.runId()))
        .isEqualTo("cancelled");

    var parentCancelled = "{\"status\":\"cancelled\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, parent.tenantId(), parent.sessionId(), parent.runId(), 2,
        parent.attemptId(), Instant.now(), UUID.randomUUID(), "run.cancelled", parentCancelled,
        sha256(parentCancelled)))).isTrue();
    var childCancelled = "{\"status\":\"cancelled\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, child.tenantId(), child.sessionId(), child.runId(), 1,
        child.attemptId(), Instant.now(), UUID.randomUUID(), "run.cancelled", childCancelled,
        sha256(childCancelled)))).isTrue();
    assertThat(fixture.jdbc().queryForList("""
        select id,status from runs where tenant_id = ? and id in (?,?)
        """, fixture.tenantId(), fixture.runId(), childRunId))
        .allSatisfy(run -> assertThat(run).containsEntry("status", "cancelled"));
  }

  @Test
  void globalRecoverySnapshotAggregatesWithoutTenantIdentifiersAndTracksResolution()
      throws Exception {
    var tenantA = fixture("scheduler-global-recovery-metrics-a");
    var workerA = UUID.randomUUID();
    tenantA.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerA, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var commandA = tenantA.scheduler().schedule(
        tenantA.tenantId(), tenantA.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(tenantA.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), commandA.tenantId(), commandA.runId(), commandA.attemptId(),
        commandA.workerId(), Instant.now()))).isTrue();

    var tenantB = fixture("scheduler-global-recovery-metrics-b");
    var workerB = UUID.randomUUID();
    tenantB.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerB, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var commandB = tenantB.scheduler().schedule(
        tenantB.tenantId(), tenantB.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(tenantB.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), commandB.tenantId(), commandB.runId(), commandB.attemptId(),
        commandB.workerId(), Instant.now()))).isTrue();

    var baseline = tenantA.scheduler().globalRecoverySloSnapshot(Duration.ofMinutes(15));

    tenantA.jdbc().update("""
        insert into recovery_incidents (
          tenant_id,incident_id,run_id,failed_attempt_id,failed_worker_id,
          failed_worker_incarnation_id,last_confirmed_healthy_at,state)
        values (?,?,?,?,?,?,now() - interval '16 minutes','waiting_capacity')
        """, tenantA.tenantId(), UUID.randomUUID(), tenantA.runId(), commandA.attemptId(),
        commandA.workerId(), commandA.workerIncarnationId());
    tenantB.jdbc().update("""
        insert into recovery_incidents (
          tenant_id,incident_id,run_id,failed_attempt_id,failed_worker_id,
          failed_worker_incarnation_id,recovery_attempt_id,last_confirmed_healthy_at,state)
        values (?,?,?,?,?,?,?,now() - interval '1 minute','recovery_requested')
        """, tenantB.tenantId(), UUID.randomUUID(), tenantB.runId(), commandB.attemptId(),
        commandB.workerId(), commandB.workerIncarnationId(), commandB.attemptId());

    var open = tenantA.scheduler().globalRecoverySloSnapshot(Duration.ofMinutes(15));
    assertThat(open)
        .extracting("openIncidents", "overdueIncidents", "waitingCapacity",
            "recoveryRequested")
        .containsExactly(
            baseline.openIncidents() + 2,
            baseline.overdueIncidents() + 1,
            baseline.waitingCapacity() + 1,
            baseline.recoveryRequested() + 1);
    assertThat(open.oldestOpenAgeMillis()).isGreaterThanOrEqualTo(960_000);

    tenantB.jdbc().update("""
        update recovery_incidents
           set state = 'recovered', resolved_at = clock_timestamp()
         where tenant_id = ? and run_id = ?
        """, tenantB.tenantId(), tenantB.runId());
    var afterResolution = tenantA.scheduler()
        .globalRecoverySloSnapshot(Duration.ofMinutes(15));
    assertThat(afterResolution)
        .extracting("openIncidents", "overdueIncidents", "waitingCapacity",
            "recoveryRequested")
        .containsExactly(
            baseline.openIncidents() + 1,
            baseline.overdueIncidents() + 1,
            baseline.waitingCapacity() + 1,
            baseline.recoveryRequested());
  }

  @Test
  void drainingWorkerRenewsOwnedLeaseButIsExcludedFromNewDispatch() throws Exception {
    var fixture = fixture("scheduler-draining-worker");
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var drainingSince = Instant.now();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, incarnationId, drainingSince,
        List.of("cloud"), 4, 0, List.of(), "0.1.0",
        false, drainingSince, drainingSince.plusSeconds(30)));
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, incarnationId, drainingSince.plusSeconds(1),
        List.of("cloud"), 4, 0, List.of(), "0.1.0"));

    var result = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));

    assertThat(result.status()).isEqualTo(ScheduleStatus.RETRY_NO_CAPACITY);
    assertThat(result.command()).isEmpty();
    assertThat(fixture.jdbc().queryForObject("""
        select accepting_work from runtime_worker_incarnations
         where worker_id = ? and incarnation_id = ?
        """, Boolean.class, workerId, incarnationId)).isFalse();
    assertThat(fixture.jdbc().queryForObject("""
        select drain_deadline from runtime_worker_incarnations
         where worker_id = ? and incarnation_id = ?
        """, Instant.class, workerId, incarnationId)).isEqualTo(drainingSince.plusSeconds(30));
  }

  @Test
  void activeWorkspaceOwnerPreventsSecondWriterDispatch() throws Exception {
    var fixture = fixture("scheduler-workspace-busy");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));
    fixture.jdbc().update("""
        insert into workspace_leases (
          tenant_id,workspace_id,owner_id,owner_epoch,fencing_token,expires_at)
        values (?,?,?,?,?,now() + interval '1 minute')
        """, fixture.tenantId(), fixture.workspaceId(), UUID.randomUUID(), 9, UUID.randomUUID());

    var result = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));

    assertThat(result.status()).isEqualTo(ScheduleStatus.RETRY_WORKSPACE_BUSY);
    assertThat(fixture.jdbc().queryForObject(
        "select status from runs where tenant_id = ? and id = ?",
        String.class, fixture.tenantId(), fixture.runId())).isEqualTo("queued");
  }

  @Test
  void workerAcceptanceIsIdempotentAndMustMatchAssignedAttempt() throws Exception {
    var fixture = fixture("scheduler-acceptance");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    var accepted = new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now());

    assertThat(fixture.scheduler().recordAcceptance(accepted)).isTrue();
    assertThat(fixture.scheduler().recordAcceptance(accepted)).isTrue();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), UUID.randomUUID(),
        command.workerId(), Instant.now()))).isFalse();
    assertThat(fixture.jdbc().queryForObject(
        "select state from run_dispatches where tenant_id = ? and run_id = ?",
        String.class, fixture.tenantId(), fixture.runId())).isEqualTo("accepted");
    assertThat(fixture.jdbc().queryForObject(
        "select status from runs where tenant_id = ? and id = ?",
        String.class, fixture.tenantId(), fixture.runId())).isEqualTo("running");
  }

  @Test
  void heartbeatRenewsOnlyTheExactFencedAssignment() throws Exception {
    var fixture = fixture("scheduler-renewal");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    fixture.jdbc().update("""
        update run_dispatches
           set lease_expires_at = now() + interval '1 second',
               workload_identity_expires_at = now() + interval '1 second'
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, command.tenantId(), command.runId(), command.attemptId());
    fixture.jdbc().update("""
        update workspace_leases set expires_at = now() + interval '1 second'
         where tenant_id = ? and workspace_id = ?
        """, command.tenantId(), command.workspaceId());

    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 1,
        List.of(new ActiveAssignmentMessage(
            command.tenantId(), command.runId(), command.attemptId(), command.workspaceId(),
            command.ownerEpoch(), command.fencingToken())),
        "0.1.0"), Duration.ofSeconds(30));

    assertThat(fixture.jdbc().queryForObject("""
        select lease_expires_at > now() + interval '20 seconds'
          from run_dispatches where tenant_id = ? and run_id = ? and attempt_id = ?
        """, Boolean.class, command.tenantId(), command.runId(), command.attemptId())).isTrue();
    assertThat(fixture.jdbc().queryForObject("""
        select expires_at > now() + interval '20 seconds'
          from workspace_leases where tenant_id = ? and workspace_id = ?
        """, Boolean.class, command.tenantId(), command.workspaceId())).isTrue();
  }

  @Test
  void heartbeatRotatesAnExpiringWorkloadIdentityWithoutChangingTheFencingOwner()
      throws Exception {
    var fixture = fixture("scheduler-identity-renewal");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    fixture.jdbc().update("""
        update run_dispatches
           set lease_expires_at = now() + interval '1 second',
               workload_identity_expires_at = now() + interval '1 second'
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, command.tenantId(), command.runId(), command.attemptId());
    fixture.jdbc().update("""
        update workspace_leases set expires_at = now() + interval '1 second'
         where tenant_id = ? and workspace_id = ?
        """, command.tenantId(), command.workspaceId());

    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 1,
        List.of(new ActiveAssignmentMessage(
            command.tenantId(), command.runId(), command.attemptId(), command.workspaceId(),
            command.ownerEpoch(), command.fencingToken())),
        "0.1.0"), Duration.ofSeconds(30));

    var renewalPayloads = fixture.jdbc().queryForList("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ?
           and event_type = 'workload.identity.renewed'
        """, String.class, command.tenantId(), command.runId());
    assertThat(renewalPayloads).hasSize(1);
    var payload = new ObjectMapper().readTree(renewalPayloads.getFirst());
    assertThat(payload.path("generation").asLong()).isEqualTo(2);
    assertThat(payload.path("worker_incarnation_id").asText())
        .isEqualTo(command.workerIncarnationId().toString());
    assertThat(payload.path("owner_epoch").asLong()).isEqualTo(command.ownerEpoch());
    assertThat(payload.path("fencing_token").asText())
        .isEqualTo(command.fencingToken().toString());
    assertThat(payload.path("workload_token").asText()).startsWith("v2.");
    var claims = new ObjectMapper().readTree(Base64.getUrlDecoder().decode(
        payload.path("workload_token").asText().split("\\.")[1]));
    assertThat(claims.path("issued_at_unix_ms").asLong())
        .isEqualTo(Instant.parse(payload.path("issued_at").asText()).toEpochMilli());
    assertThat(claims.path("expires_at_unix_ms").asLong())
        .isEqualTo(Instant.parse(payload.path("lease_expires_at").asText()).toEpochMilli());
    assertThat(fixture.jdbc().queryForObject("""
        select workload_identity_generation from run_dispatches
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, Long.class, command.tenantId(), command.runId(), command.attemptId())).isEqualTo(2L);

    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 1,
        List.of(new ActiveAssignmentMessage(
            command.tenantId(), command.runId(), command.attemptId(), command.workspaceId(),
            command.ownerEpoch(), command.fencingToken())),
        "0.1.0"), Duration.ofSeconds(30));
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ?
           and event_type = 'workload.identity.renewed'
        """, Integer.class, command.tenantId(), command.runId())).isOne();
  }

  @Test
  void expiredUnacceptedDispatchIsLostAndRunIsRequeued() throws Exception {
    var fixture = fixture("scheduler-requeue");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    expireAssignment(fixture, workerId);

    var result = fixture.scheduler().reconcileExpired();

    // reconcileExpired sweeps every tenant, so an exact tally is a claim about
    // what other tests left behind, not about this one. It held only while the
    // suite happened to run in an order that left nothing else expired, and
    // stopped holding as the suite grew. The three assertions below are scoped
    // to this fixture and are what actually prove this run was requeued.
    assertThat(result.requeued()).isGreaterThanOrEqualTo(1);
    assertThat(fixture.jdbc().queryForObject("""
        select state from run_dispatches
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, String.class, fixture.tenantId(), fixture.runId(), command.attemptId()))
        .isEqualTo("lost");
    assertThat(fixture.jdbc().queryForObject("""
        select status from runs where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), fixture.runId())).isEqualTo("queued");
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.queued'
        """, Integer.class, fixture.tenantId(), fixture.runId())).isEqualTo(2);

    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var retry = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));
    assertThat(retry.status()).isEqualTo(ScheduleStatus.DISPATCHED);
    assertThat(retry.command().orElseThrow().attemptId()).isNotEqualTo(command.attemptId());
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_dispatches where tenant_id = ? and run_id = ?
        """, Integer.class, fixture.tenantId(), fixture.runId())).isEqualTo(2);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from recovery_incidents where tenant_id = ? and run_id = ?
        """, Integer.class, fixture.tenantId(), fixture.runId())).isZero();
  }

  @Test
  void runNoWorkerEverAcceptsFailsTerminallyInsteadOfBeingRequeuedForever() throws Exception {
    // A Worker that refuses an assignment (for example a Skill declaring a Tool
    // the trusted registry does not hold) terminates the JetStream message and
    // reports nothing. Without a bound the reconciler requeues the same poison
    // assignment forever, and every redispatch renews the workspace lease, so
    // the workspace never returns to 'ready' and every later Run in it is
    // refused with "run target is not available".
    var fixture = fixture("scheduler-never-accepted");
    var workerId = UUID.randomUUID();
    var status = "queued";
    var dispatches = 0;
    for (var round = 0; round < 12 && "queued".equals(status); round++) {
      fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
          1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
          List.of(), "0.1.0"), Duration.ofSeconds(30));
      var scheduled = fixture.scheduler().schedule(
          fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15));
      if (scheduled.command().isEmpty()) {
        break;
      }
      dispatches++;
      // The Worker never accepts and never emits an event, so the dispatch just
      // ages out exactly as it does natively.
      expireAssignment(fixture, workerId);
      fixture.scheduler().reconcileExpired();
      status = fixture.jdbc().queryForObject("""
          select status from runs where tenant_id = ? and id = ?
          """, String.class, fixture.tenantId(), fixture.runId());
    }

    assertThat(status)
        .as("a Run no Worker ever accepts must stop being requeued")
        .isNotEqualTo("queued");
    assertThat(dispatches)
        .as("redispatch must be bounded")
        .isLessThanOrEqualTo(8);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_events
         where tenant_id = ? and run_id = ? and type = 'run.failed'
        """, Integer.class, fixture.tenantId(), fixture.runId()))
        .as("the terminal failure must be durably recorded")
        .isOne();
    assertThat(fixture.jdbc().queryForObject("""
        select finished_at is not null from runs where tenant_id = ? and id = ?
        """, Boolean.class, fixture.tenantId(), fixture.runId())).isTrue();
    assertThat(fixture.jdbc().queryForObject("""
        select state from workspaces where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), fixture.workspaceId()))
        .as("the workspace must be released so later Runs can be scheduled")
        .isEqualTo("ready");
  }

  @Test
  void expiredAcceptedDispatchBecomesIndeterminateWithoutReplay() throws Exception {
    var fixture = fixture("scheduler-indeterminate");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    expireAssignment(fixture, workerId);

    var result = fixture.scheduler().reconcileExpired();

    assertThat(result).isEqualTo(new ReconcileResult(0, 1));
    assertThat(fixture.jdbc().queryForObject("""
        select status from runs where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), fixture.runId())).isEqualTo("indeterminate");
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.queued'
        """, Integer.class, fixture.tenantId(), fixture.runId())).isOne();
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_events
         where tenant_id = ? and run_id = ? and type = 'run.indeterminate'
        """, Integer.class, fixture.tenantId(), fixture.runId())).isOne();
  }

  @Test
  void expiredAcceptedDispatchWithSafeCheckpointGetsANewFencedRecoveryAttempt() throws Exception {
    var fixture = fixture("scheduler-safe-recovery");
    var firstWorkerId = UUID.randomUUID();
    var secondWorkerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), firstWorkerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), secondWorkerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var original = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    var replacementWorkerId = original.workerId().equals(firstWorkerId)
        ? secondWorkerId : firstWorkerId;
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.attemptId(),
        original.workerId(), Instant.now()));
    var startedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 1,
        original.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", startedPayload,
        sha256(startedPayload)))).isTrue();
    var checkpointPayload = "{\"run_id\":\"%s\",\"sequence\":1}".formatted(original.runId());
    var storedDigest = sha256("compressed-checkpoint");
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        2, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        original.attemptId(), original.ownerEpoch(), original.fencingToken(), 1, "running",
        "a".repeat(64), "b".repeat(64), null, "checkpoint://sha256/" + storedDigest, "zstd",
        sha256(checkpointPayload), storedDigest, checkpointPayload.length(), 600_000,
        Instant.now()))).isTrue();
    expireAssignment(fixture, original.workerId());

    var result = fixture.scheduler().reconcileExpired();

    assertThat(result).isEqualTo(new ReconcileResult(0, 1, 0));
    var dispatches = fixture.jdbc().queryForList("""
        select attempt_id,worker_id,owner_epoch,fencing_token,state
          from run_dispatches where tenant_id = ? and run_id = ? order by requested_at
        """, fixture.tenantId(), fixture.runId());
    assertThat(dispatches).hasSize(2);
    assertThat(dispatches.getFirst()).containsEntry("state", "lost");
    assertThat(dispatches.getLast())
        .containsEntry("state", "requested")
        .containsEntry("worker_id", replacementWorkerId)
        .containsEntry("owner_epoch", original.ownerEpoch() + 1);
    assertThat(dispatches.getLast().get("fencing_token")).isNotEqualTo(original.fencingToken());
    assertThat(fixture.jdbc().queryForMap("""
        select status,current_attempt_id,last_sequence from runs where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("status", "running")
        .containsEntry("current_attempt_id", dispatches.getLast().get("attempt_id"))
        .containsEntry("last_sequence", 1L);
    var recoveryPayload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.recovery.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(recoveryPayload.path("execution").path("worker_id").asText())
        .isEqualTo(replacementWorkerId.toString());
    assertThat(recoveryPayload.path("execution").path("owner_epoch").asLong())
        .isEqualTo(original.ownerEpoch() + 1);
    assertThat(recoveryPayload.path("checkpoint").path("attempt_id").asText())
        .isEqualTo(original.attemptId().toString());
    assertThat(recoveryPayload.path("checkpoint").path("payload_ref").asText())
        .isEqualTo("checkpoint://sha256/" + storedDigest);
    assertThat(recoveryPayload.path("checkpoint").has("payload_base64")).isFalse();
    assertThat(recoveryPayload.path("checkpoint").path("payload_encoding").asText())
        .isEqualTo("zstd");
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_events
         where tenant_id = ? and run_id = ? and type = 'run.indeterminate'
        """, Integer.class, fixture.tenantId(), fixture.runId())).isZero();

    var incident = fixture.jdbc().queryForMap("""
        select state,failed_attempt_id,recovery_attempt_id,last_confirmed_healthy_at,
               detected_at,resolved_at
          from recovery_incidents where tenant_id = ? and run_id = ?
        """, fixture.tenantId(), fixture.runId());
    var replacementAttemptId = (UUID) dispatches.getLast().get("attempt_id");
    assertThat(incident)
        .containsEntry("state", "recovery_requested")
        .containsEntry("failed_attempt_id", original.attemptId())
        .containsEntry("recovery_attempt_id", replacementAttemptId)
        .containsEntry("resolved_at", null);
    assertThat(incident.get("last_confirmed_healthy_at"))
        .isNotNull()
        .isInstanceOf(java.sql.Timestamp.class);
    assertThat(incident.get("detected_at")).isNotNull();
    var openSnapshot = fixture.scheduler().recoverySloSnapshot(
        fixture.tenantId(), Duration.ofMinutes(15));
    assertThat(openSnapshot)
        .extracting("openIncidents", "overdueIncidents", "waitingCapacity",
            "recoveryRequested")
        .containsExactly(1, 0, 0, 1);
    assertThat(openSnapshot.oldestOpenAgeMillis()).isGreaterThanOrEqualTo(60_000);

    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), fixture.tenantId(), fixture.runId(), replacementAttemptId,
        replacementWorkerId, Instant.now()))).isTrue();
    var restoredPayload = "{\"checkpoint_digest\":\"%s\"}".formatted(storedDigest);
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 2,
        replacementAttemptId, Instant.now(), UUID.randomUUID(), "run.restored", restoredPayload,
        sha256(restoredPayload)))).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select state,resolved_at from recovery_incidents where tenant_id = ? and run_id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("state", "recovered")
        .doesNotContainEntry("resolved_at", null);
    assertThat(fixture.scheduler().recoverySloSnapshot(
        fixture.tenantId(), Duration.ofMinutes(15)))
        .isEqualTo(new RecoverySloSnapshot(0, 0, 0, 0, 0));
  }

  @Test
  void safeRecoveryRebindsAPendingSteerIntoTheReplacementCommand() throws Exception {
    var fixture = fixture("scheduler-steering-recovery");
    var firstWorkerId = UUID.randomUUID();
    var secondWorkerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), firstWorkerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), secondWorkerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var original = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.attemptId(),
        original.workerId(), Instant.now()));
    var started = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 1,
        original.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", started,
        sha256(started)))).isTrue();
    var checkpointPayload = "{\"run_id\":\"%s\",\"sequence\":1}"
        .formatted(original.runId()).getBytes(StandardCharsets.UTF_8);
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        original.attemptId(), original.ownerEpoch(), original.fencingToken(), 1, "running",
        "a".repeat(64), "b".repeat(64), checkpointPayload,
        sha256(new String(checkpointPayload, StandardCharsets.UTF_8)), Instant.now()))).isTrue();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var runs = new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
    var steering = runs.requestSteering(
        fixture.tenantId(), fixture.applicationId(), fixture.runId(), "steer-before-recovery",
        "Continue with the recovered worker.", Instant.now());
    expireAssignment(fixture, original.workerId());

    assertThat(fixture.scheduler().reconcileExpired())
        .isEqualTo(new ReconcileResult(0, 1, 0));

    var recovery = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.recovery.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    var replacementAttemptId = UUID.fromString(
        recovery.path("execution").path("attempt_id").asText());
    var replacementWorkerId = UUID.fromString(
        recovery.path("execution").path("worker_id").asText());
    var replacementIncarnationId = UUID.fromString(
        recovery.path("execution").path("worker_incarnation_id").asText());
    assertThat(recovery.path("schema_version").asInt()).isEqualTo(3);
    assertThat(recovery.path("steering").path("steering_id").asText())
        .isEqualTo(steering.steeringId().toString());
    assertThat(recovery.path("steering").path("attempt_id").asText())
        .isEqualTo(replacementAttemptId.toString());
    assertThat(recovery.path("steering").path("worker_id").asText())
        .isEqualTo(replacementWorkerId.toString());
    assertThat(recovery.path("steering").path("worker_incarnation_id").asText())
        .isEqualTo(replacementIncarnationId.toString());
    assertThat(recovery.path("steering").path("input").asText())
        .isEqualTo("Continue with the recovered worker.");
    assertThat(recovery.path("steering").path("input_digest").asText())
        .isEqualTo(sha256("Continue with the recovered worker."));
    assertThat(fixture.jdbc().queryForMap("""
        select attempt_id,worker_id,worker_incarnation_id,state
          from run_steering_commands
         where tenant_id = ? and steering_id = ?
        """, fixture.tenantId(), steering.steeringId()))
        .containsEntry("attempt_id", replacementAttemptId)
        .containsEntry("worker_id", replacementWorkerId)
        .containsEntry("worker_incarnation_id", replacementIncarnationId)
        .containsEntry("state", "pending");
  }

  @Test
  void safeCheckpointWithoutReplacementCapacityCreatesAnOverdueWaitingIncident()
      throws Exception {
    var fixture = fixture("scheduler-recovery-waiting-capacity");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var original = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.attemptId(),
        original.workerId(), Instant.now()))).isTrue();
    var startedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 1,
        original.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", startedPayload,
        sha256(startedPayload)))).isTrue();
    var checkpointPayload = "{\"run_id\":\"%s\",\"sequence\":1}".formatted(original.runId());
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        original.attemptId(), original.ownerEpoch(), original.fencingToken(), 1, "running",
        "a".repeat(64), "b".repeat(64), checkpointPayload.getBytes(StandardCharsets.UTF_8),
        sha256(checkpointPayload), Instant.now()))).isTrue();
    expireAssignment(fixture, workerId);
    fixture.jdbc().update("""
        update runtime_worker_incarnations
           set last_heartbeat = now() - interval '16 minutes',
               last_heartbeat_received_at = now() - interval '16 minutes'
         where worker_id = ? and incarnation_id = ?
        """, original.workerId(), original.workerIncarnationId());

    assertThat(fixture.scheduler().reconcileExpired()).isEqualTo(new ReconcileResult(0, 0, 0));
    assertThat(fixture.jdbc().queryForMap("""
        select state,failed_attempt_id,recovery_attempt_id,resolved_at
          from recovery_incidents where tenant_id = ? and run_id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("state", "waiting_capacity")
        .containsEntry("failed_attempt_id", original.attemptId())
        .containsEntry("recovery_attempt_id", null)
        .containsEntry("resolved_at", null);
    var overdueSnapshot = fixture.scheduler().recoverySloSnapshot(
        fixture.tenantId(), Duration.ofMinutes(15));
    assertThat(overdueSnapshot)
        .extracting("openIncidents", "overdueIncidents", "waitingCapacity",
            "recoveryRequested")
        .containsExactly(1, 1, 1, 0);
    assertThat(overdueSnapshot.oldestOpenAgeMillis()).isGreaterThanOrEqualTo(960_000);
  }

  @Test
  void scaledWorkerLossDrillRequestsRecoveryWithinTheReconcileBudget() throws Exception {
    var fixture = fixture("scheduler-scaled-worker-loss-drill");
    var originalWorkerId = UUID.randomUUID();
    var replacementWorkerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), originalWorkerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofMillis(750));
    var original = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofMillis(750), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.attemptId(),
        original.workerId(), Instant.now()))).isTrue();
    var startedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 1,
        original.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", startedPayload,
        sha256(startedPayload)))).isTrue();
    var checkpointPayload = "{\"run_id\":\"%s\",\"sequence\":1}".formatted(original.runId());
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        original.attemptId(), original.ownerEpoch(), original.fencingToken(), 1, "running",
        "a".repeat(64), "b".repeat(64), checkpointPayload.getBytes(StandardCharsets.UTF_8),
        sha256(checkpointPayload), Instant.now()))).isTrue();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), replacementWorkerId, Instant.now(), List.of("cloud"), 1, 0,
        List.of(), "0.1.0"), Duration.ofMillis(750));

    var startedAt = System.nanoTime();
    var deadline = startedAt + Duration.ofSeconds(3).toNanos();
    var incidentRecorded = false;
    while (System.nanoTime() < deadline && !incidentRecorded) {
      fixture.scheduler().reconcileExpired(
          Duration.ofMillis(750), Duration.ofSeconds(15));
      incidentRecorded = fixture.jdbc().queryForObject("""
          select count(*) = 1 from recovery_incidents
           where tenant_id = ? and run_id = ? and state = 'recovery_requested'
          """, Boolean.class, fixture.tenantId(), fixture.runId());
      if (!incidentRecorded) {
        Thread.sleep(25);
      }
    }

    assertThat(incidentRecorded).isTrue();
    assertThat(Duration.ofNanos(System.nanoTime() - startedAt)).isLessThan(Duration.ofSeconds(2));
    assertThat(fixture.jdbc().queryForMap("""
        select state,failed_attempt_id,recovery_attempt_id
          from recovery_incidents where tenant_id = ? and run_id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("state", "recovery_requested")
        .containsEntry("failed_attempt_id", original.attemptId());
  }

  @Test
  void restartedWorkerCanRecoverItsOldAttemptThroughANewIncarnation() throws Exception {
    var fixture = fixture("scheduler-same-worker-new-incarnation");
    var workerId = UUID.randomUUID();
    var originalIncarnationId = UUID.randomUUID();
    var replacementIncarnationId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, originalIncarnationId, Instant.now(),
        List.of("cloud"), 4, 0, List.of(), "0.1.0"), Duration.ofSeconds(30));
    var original = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    assertThat(fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        2, UUID.randomUUID(), original.tenantId(), original.runId(), original.attemptId(),
        workerId, originalIncarnationId, Instant.now()))).isTrue();
    var startedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 1,
        original.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", startedPayload,
        sha256(startedPayload)))).isTrue();
    var checkpointPayload = "{\"run_id\":\"%s\",\"sequence\":1}".formatted(original.runId());
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        original.attemptId(), original.ownerEpoch(), original.fencingToken(), 1, "running",
        "a".repeat(64), "b".repeat(64), checkpointPayload.getBytes(StandardCharsets.UTF_8),
        sha256(checkpointPayload), Instant.now()))).isTrue();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, replacementIncarnationId, Instant.now().plusMillis(1),
        List.of("cloud"), 4, 0, List.of(), "0.1.0"), Duration.ofSeconds(30));
    expireAssignment(fixture, workerId);

    assertThat(fixture.scheduler().reconcileExpired()).isEqualTo(new ReconcileResult(0, 1, 0));
    var recovery = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.recovery.requested'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(recovery.path("execution").path("worker_id").asText())
        .isEqualTo(workerId.toString());
    assertThat(recovery.path("execution").path("worker_incarnation_id").asText())
        .isEqualTo(replacementIncarnationId.toString());
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_dispatches
         where tenant_id = ? and run_id = ? and worker_id = ?
        """, Integer.class, fixture.tenantId(), fixture.runId(), workerId)).isEqualTo(2);
  }

  @Test
  void lateHeartbeatFromAnOldIncarnationCannotReclaimTheStableWorker() throws Exception {
    var fixture = fixture("scheduler-old-incarnation-heartbeat");
    var workerId = UUID.randomUUID();
    var oldIncarnationId = UUID.randomUUID();
    var newIncarnationId = UUID.randomUUID();
    var firstSeen = Instant.now();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, oldIncarnationId, firstSeen,
        List.of("cloud"), 4, 0, List.of(), "0.1.0"));
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, newIncarnationId, firstSeen.plusSeconds(1),
        List.of("cloud"), 4, 0, List.of(), "0.1.0"));

    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        2, UUID.randomUUID(), workerId, oldIncarnationId, firstSeen.plusSeconds(2),
        List.of("cloud"), 4, 0, List.of(), "0.1.0"));

    assertThat(fixture.jdbc().queryForObject("""
        select current_incarnation_id from runtime_workers where id = ?
        """, UUID.class, workerId)).isEqualTo(newIncarnationId);
    assertThat(fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow().workerIncarnationId()).isEqualTo(newIncarnationId);
  }

  @Test
  void recoveredApprovalIsReboundToTheReplacementAttemptBeforeItCanBeDecided() throws Exception {
    var fixture = fixture("scheduler-approval-recovery");
    var firstWorkerId = UUID.randomUUID();
    var secondWorkerId = UUID.randomUUID();
    for (var workerId : List.of(firstWorkerId, secondWorkerId)) {
      fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
          1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
          List.of(), "0.1.0"), Duration.ofSeconds(30));
    }
    var original = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.attemptId(),
        original.workerId(), Instant.now()));
    var approvalId = UUID.randomUUID();
    var bindingDigest = "f".repeat(64);
    var approvalPayload = """
        {"approval":{"approval_id":"%s","execution":{"call":{"id":"call_shell","name":"shell","arguments":{"command":"cargo test"}},"effect":"unknown","sandbox":"kata","binding_digest":"%s"}},"status":"waiting_approval"}
        """.formatted(approvalId, bindingDigest).strip();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 1,
        original.attemptId(), Instant.now(), UUID.randomUUID(), "approval.required",
        approvalPayload, sha256(approvalPayload)))).isTrue();
    var checkpointPayload = "{\"run_id\":\"%s\",\"sequence\":1}".formatted(original.runId());
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        original.attemptId(), original.ownerEpoch(), original.fencingToken(), 1,
        "waiting_approval", "a".repeat(64), "b".repeat(64),
        checkpointPayload.getBytes(StandardCharsets.UTF_8), sha256(checkpointPayload),
        Instant.now()))).isTrue();
    expireAssignment(fixture, original.workerId());

    assertThat(fixture.scheduler().reconcileExpired()).isEqualTo(new ReconcileResult(0, 1, 0));
    var replacement = fixture.jdbc().queryForMap("""
        select attempt_id,worker_id from run_dispatches
         where tenant_id = ? and run_id = ? and state = 'requested'
        """, fixture.tenantId(), fixture.runId());
    var replacementAttemptId = (UUID) replacement.get("attempt_id");
    var replacementWorkerId = (UUID) replacement.get("worker_id");
    assertThat(fixture.jdbc().queryForMap("""
        select attempt_id,worker_id,status from approvals where tenant_id = ? and id = ?
        """, fixture.tenantId(), approvalId))
        .containsEntry("attempt_id", replacementAttemptId)
        .containsEntry("worker_id", replacementWorkerId)
        .containsEntry("status", "pending");

    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), fixture.tenantId(), fixture.runId(), replacementAttemptId,
        replacementWorkerId, Instant.now()));
    var restoredPayload = "{\"checkpoint_digest\":\"%s\"}".formatted("c".repeat(64));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 2,
        replacementAttemptId, Instant.now(), UUID.randomUUID(), "run.restored", restoredPayload,
        sha256(restoredPayload)))).isTrue();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, original.tenantId(), original.sessionId(), original.runId(), 3,
        replacementAttemptId, Instant.now(), UUID.randomUUID(), "approval.rebound",
        approvalPayload, sha256(approvalPayload)))).isTrue();
    assertThat(fixture.scheduler().findToolExecution(
        original.tenantId(), original.runId(), replacementAttemptId, "call_shell").orElseThrow())
        .extracting("state", "bindingDigest")
        .containsExactly("planned", bindingDigest);

    var approvals = new JdbcApprovalRepository(
        fixture.jdbc(), new TransactionTemplate(
            new DataSourceTransactionManager(fixture.jdbc().getDataSource())));
    approvals.decide(fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
        approvalId, 1, ApprovalDecision.ALLOW_ONCE, "recovered", "user-42", Instant.now()));
    var decision = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'tool.approval.decided'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(decision.path("attempt_id").asText()).isEqualTo(replacementAttemptId.toString());
    assertThat(decision.path("worker_id").asText()).isEqualTo(replacementWorkerId.toString());

    var replacementDispatch = fixture.jdbc().queryForMap("""
        select worker_incarnation_id,owner_epoch,fencing_token
          from run_dispatches
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, fixture.tenantId(), fixture.runId(), replacementAttemptId);
    var replacementCheckpointPayload = "{\"run_id\":\"%s\",\"sequence\":3}"
        .formatted(original.runId());
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), original.tenantId(), original.runId(), original.sessionId(),
        replacementAttemptId, (Long) replacementDispatch.get("owner_epoch"),
        (UUID) replacementDispatch.get("fencing_token"), 3, "waiting_approval",
        "c".repeat(64), "b".repeat(64),
        replacementCheckpointPayload.getBytes(StandardCharsets.UTF_8),
        sha256(replacementCheckpointPayload), Instant.now()))).isTrue();
    var finalWorkerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), finalWorkerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    expireAssignment(fixture, replacementWorkerId);

    assertThat(fixture.scheduler().reconcileExpired()).isEqualTo(new ReconcileResult(0, 1, 0));
    var finalAttempt = fixture.jdbc().queryForMap("""
        select attempt_id,worker_id,worker_incarnation_id
          from run_dispatches
         where tenant_id = ? and run_id = ? and state = 'requested'
        """, fixture.tenantId(), fixture.runId());
    assertThat(fixture.jdbc().queryForMap("""
        select attempt_id,worker_id,worker_incarnation_id,status
          from approvals where tenant_id = ? and id = ?
        """, fixture.tenantId(), approvalId))
        .containsEntry("attempt_id", finalAttempt.get("attempt_id"))
        .containsEntry("worker_id", finalAttempt.get("worker_id"))
        .containsEntry("worker_incarnation_id", finalAttempt.get("worker_incarnation_id"))
        .containsEntry("status", "approved");
    var replayedDecision = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'tool.approval.decided'
         order by created_at desc limit 1
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(replayedDecision.path("attempt_id").asText())
        .isEqualTo(finalAttempt.get("attempt_id").toString());
    assertThat(replayedDecision.path("approval_id").asText()).isEqualTo(approvalId.toString());
    assertThat(replayedDecision.path("approval_version").asInt()).isEqualTo(2);
    assertThat(replayedDecision.path("decision").asText()).isEqualTo("allow_once");
  }

  @Test
  void durableCheckpointIsIdempotentAndEligibleOnlyAtTheCurrentRunSequence() throws Exception {
    var fixture = fixture("scheduler-checkpoint");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var startedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started", startedPayload,
        sha256(startedPayload)))).isTrue();
    var payload = "{\"run_id\":\"%s\",\"sequence\":1}".formatted(command.runId());
    var checkpoint = new RunCheckpointMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.sessionId(),
        command.attemptId(), command.ownerEpoch(), command.fencingToken(), 1, "running",
        "a".repeat(64), "b".repeat(64), payload.getBytes(StandardCharsets.UTF_8),
        sha256(payload), Instant.now());

    assertThat(fixture.scheduler().recordCheckpoint(checkpoint)).isTrue();
    assertThat(fixture.scheduler().recordCheckpoint(checkpoint)).isTrue();
    assertThat(fixture.scheduler().assessRecovery(
        command.tenantId(), command.runId(), command.attemptId()))
        .isEqualTo(RecoveryEligibility.SAFE);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_checkpoints
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, Integer.class, command.tenantId(), command.runId(), command.attemptId())).isOne();

    var corrupt = new RunCheckpointMessage(
        checkpoint.schemaVersion(), UUID.randomUUID(), checkpoint.tenantId(), checkpoint.runId(),
        checkpoint.sessionId(), checkpoint.attemptId(), checkpoint.ownerEpoch(),
        checkpoint.fencingToken(), checkpoint.sequence(), checkpoint.status(),
        checkpoint.kernelDigest(), checkpoint.toolCatalogDigest(), checkpoint.payload(),
        "c".repeat(64), Instant.now());
    assertThat(fixture.scheduler().recordCheckpoint(corrupt)).isFalse();

    var deltaPayload = "{\"text\":\"newer than checkpoint\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "model.output.delta", deltaPayload,
        sha256(deltaPayload)))).isTrue();
    assertThat(fixture.scheduler().assessRecovery(
        command.tenantId(), command.runId(), command.attemptId()))
        .isEqualTo(RecoveryEligibility.STALE);
  }

  @Test
  void expiredWorkerAfterNonIdempotentToolStartRecordsTheAmbiguousBinding() throws Exception {
    var fixture = fixture("scheduler-ambiguous-tool");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var bindingDigest = "e".repeat(64);
    var executionPayload = """
        {"execution":{"call":{"id":"call_charge","name":"charge","arguments":{"amount":42}},"effect":"non_idempotent","sandbox":"restricted_container","binding_digest":"%s"}}
        """.formatted(bindingDigest).strip();
    for (var eventType : List.of("tool.execution.requested", "tool.execution.started")) {
      var sequence = "tool.execution.requested".equals(eventType) ? 1 : 2;
      assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
          UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), sequence,
          command.attemptId(), Instant.now(), UUID.randomUUID(), eventType, executionPayload,
          sha256(executionPayload)))).isTrue();
    }
    var checkpointPayload = "{\"sequence\":2,\"tool_call_id\":\"call_charge\"}";
    assertThat(fixture.scheduler().recordCheckpoint(new RunCheckpointMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.sessionId(),
        command.attemptId(), command.ownerEpoch(), command.fencingToken(), 2, "running",
        "a".repeat(64), "b".repeat(64), checkpointPayload.getBytes(StandardCharsets.UTF_8),
        sha256(checkpointPayload), Instant.now()))).isTrue();
    assertThat(fixture.scheduler().assessRecovery(
        command.tenantId(), command.runId(), command.attemptId()))
        .isEqualTo(RecoveryEligibility.AMBIGUOUS_SIDE_EFFECT);
    expireAssignment(fixture, workerId);

    assertThat(fixture.scheduler().reconcileExpired()).isEqualTo(new ReconcileResult(0, 1));

    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from run_events
         where tenant_id = ? and run_id = ? and type = 'run.indeterminate'
        """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("reason").asText()).isEqualTo("ambiguous_non_idempotent_tool");
    assertThat(payload.path("tool_call_id").asText()).isEqualTo("call_charge");
    assertThat(payload.path("binding_digest").asText()).isEqualTo(bindingDigest);
    assertThat(payload.path("replay_safe").asBoolean()).isFalse();
  }

  @Test
  void matchingKernelEventIsPersistedOnceAndAdvancesRunSequence() throws Exception {
    var fixture = fixture("scheduler-kernel-event");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var event = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started",
        "{\"status\":\"running\"}",
        "409443a6ee5aa296dccd6c0d193e214568daa0053b66155fba8adca995b7823d");

    assertThat(fixture.scheduler().recordRunEvent(event)).isTrue();
    assertThat(fixture.scheduler().recordRunEvent(event)).isTrue();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        event.eventId(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started",
        "{\"status\":\"running\"}",
        "409443a6ee5aa296dccd6c0d193e214568daa0053b66155fba8adca995b7823d"))).isFalse();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        UUID.randomUUID(), Instant.now(), UUID.randomUUID(), "run.started",
        "{\"status\":\"running\"}",
        "409443a6ee5aa296dccd6c0d193e214568daa0053b66155fba8adca995b7823d"))).isFalse();
    assertThat(fixture.jdbc().queryForObject("""
        select last_sequence from runs where tenant_id = ? and id = ?
        """, Long.class, fixture.tenantId(), fixture.runId())).isOne();
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_events where tenant_id = ? and run_id = ?
        """, Integer.class, fixture.tenantId(), fixture.runId())).isOne();
  }

  @Test
  void trustedNativeToolExecutionIsAcceptedByTheDurableLedger() throws Exception {
    var fixture = fixture("scheduler-trusted-native-tool");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var bindingDigest = "f".repeat(64);
    var payload = """
        {"execution":{"call":{"id":"call_read_native","name":"workspace.read_text","arguments":{"path":"README.txt"}},"effect":"pure","sandbox":"trusted_native","binding_digest":"%s"}}
        """.formatted(bindingDigest).strip();

    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.execution.requested",
        payload, sha256(payload)))).isTrue();
    assertThat(fixture.scheduler().findToolExecution(
        command.tenantId(), command.runId(), command.attemptId(), "call_read_native").orElseThrow())
        .extracting("state", "effect", "sandbox", "bindingDigest")
        .containsExactly("planned", "pure", "trusted_native", bindingDigest);
  }

  @Test
  void toolExecutionLedgerAdvancesOnlyForTheExactBoundCall() throws Exception {
    var fixture = fixture("scheduler-tool-ledger");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var bindingDigest = "c".repeat(64);
    var executionPayload = """
        {"execution":{"call":{"id":"call_write","name":"write_api","arguments":{"value":42}},"effect":"non_idempotent","sandbox":"restricted_container","binding_digest":"%s"}}
        """.formatted(bindingDigest).strip();
    var requested = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.execution.requested",
        executionPayload, sha256(executionPayload));

    assertThat(fixture.scheduler().recordRunEvent(requested)).isTrue();
    assertThat(fixture.scheduler().findToolExecution(
        command.tenantId(), command.runId(), command.attemptId(), "call_write").orElseThrow())
        .extracting("state", "effect", "bindingDigest")
        .containsExactly("planned", "non_idempotent", bindingDigest);

    var started = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.execution.started",
        executionPayload, sha256(executionPayload));
    assertThat(fixture.scheduler().recordRunEvent(started)).isTrue();
    assertThat(fixture.scheduler().findToolExecution(
        command.tenantId(), command.runId(), command.attemptId(), "call_write").orElseThrow())
        .extracting("state", "startedEventId")
        .containsExactly("started", started.eventId());

    var wrongResultPayload = """
        {"tool_call_id":"call_write","binding_digest":"%s","content":{"ok":true},"is_error":false}
        """.formatted("d".repeat(64)).strip();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 3,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.result",
        wrongResultPayload, sha256(wrongResultPayload)))).isFalse();

    var resultPayload = """
        {"tool_call_id":"call_write","binding_digest":"%s","content":{"ok":true},"is_error":false}
        """.formatted(bindingDigest).strip();
    var result = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 3,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.result",
        resultPayload, sha256(resultPayload));
    assertThat(fixture.scheduler().recordRunEvent(result)).isTrue();
    assertThat(fixture.scheduler().findToolExecution(
        command.tenantId(), command.runId(), command.attemptId(), "call_write").orElseThrow())
        .extracting("state", "resultEventId")
        .containsExactly("completed", result.eventId());
  }

  @Test
  void approvalRequiredIsPersistedWithItsExactAttemptAndMovesRunToWaiting() throws Exception {
    var fixture = fixture("scheduler-approval-required");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var approvalId = UUID.randomUUID();
    var bindingDigest = "a".repeat(64);
    var payload = """
        {"approval":{"approval_id":"%s","execution":{"call":{"id":"call_shell","name":"shell","arguments":{"command":"cargo test"}},"effect":"unknown","sandbox":"kata","binding_digest":"%s"}},"status":"waiting_approval"}
        """.formatted(approvalId, bindingDigest).strip();
    var event = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "approval.required", payload,
        sha256(payload));

    assertThat(fixture.scheduler().recordRunEvent(event)).isTrue();
    assertThat(fixture.scheduler().recordRunEvent(event)).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select status,current_attempt_id,last_sequence from runs
         where tenant_id = ? and id = ?
        """, command.tenantId(), command.runId()))
        .containsEntry("status", "waiting_approval")
        .containsEntry("current_attempt_id", command.attemptId())
        .containsEntry("last_sequence", 1L);
    assertThat(fixture.jdbc().queryForMap("""
        select run_id,attempt_id,worker_id,tool_call_id,binding_digest,status,version
          from approvals where tenant_id = ? and id = ?
        """, command.tenantId(), approvalId))
        .containsEntry("run_id", command.runId())
        .containsEntry("attempt_id", command.attemptId())
        .containsEntry("worker_id", command.workerId())
        .containsEntry("tool_call_id", "call_shell")
        .containsEntry("binding_digest", bindingDigest)
        .containsEntry("status", "pending")
        .containsEntry("version", 1);

    var runs = new JdbcRunRepository(
        fixture.jdbc(), new TransactionTemplate(
            new DataSourceTransactionManager(fixture.jdbc().getDataSource())));
    assertThat(runs.requestCancellation(command.tenantId(), command.runId(), Instant.now()))
        .isEqualTo(RunStatus.WAITING_APPROVAL);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.cancellation.requested'
        """, Integer.class, command.tenantId(), command.runId())).isOne();
  }

  @Test
  void approvalDecisionIsVersionedTenantBoundAndPublishedToTheCurrentWorkerOnce() throws Exception {
    var fixture = fixture("scheduler-approval-decision");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var approvalId = UUID.randomUUID();
    var bindingDigest = "b".repeat(64);
    var payload = """
        {"approval":{"approval_id":"%s","execution":{"call":{"id":"call_shell","name":"shell","arguments":{"command":"cargo test"}},"effect":"unknown","sandbox":"kata","binding_digest":"%s"}},"status":"waiting_approval"}
        """.formatted(approvalId, bindingDigest).strip();
    fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "approval.required", payload,
        sha256(payload)));
    var transactions = new TransactionTemplate(
        new DataSourceTransactionManager(fixture.jdbc().getDataSource()));
    var approvals = new JdbcApprovalRepository(fixture.jdbc(), transactions);
    var decidedAt = Instant.parse("2026-08-01T04:00:00Z");

    assertThatThrownBy(() -> approvals.decide(
        fixture.tenantId(), UUID.randomUUID(), new DecideApprovalCommand(
            approvalId, 1, ApprovalDecision.DENY, "not my application",
            "other-application", decidedAt)))
        .isInstanceOf(ApprovalNotFound.class);

    var decided = approvals.decide(
        fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
        approvalId, 1, ApprovalDecision.ALLOW_ONCE, null, "user-42", decidedAt));

    assertThat(decided.version()).isEqualTo(2);
    assertThat(decided.status().name()).isEqualTo("APPROVED");
    var outbox = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'tool.approval.decided'
        """, String.class, command.tenantId(), command.runId()));
    assertThat(outbox.path("worker_id").asText()).isEqualTo(command.workerId().toString());
    assertThat(outbox.path("worker_incarnation_id").asText())
        .isEqualTo(command.workerIncarnationId().toString());
    assertThat(outbox.path("schema_version").asInt()).isEqualTo(2);
    assertThat(outbox.path("attempt_id").asText()).isEqualTo(command.attemptId().toString());
    assertThat(outbox.path("approval_id").asText()).isEqualTo(approvalId.toString());
    assertThat(outbox.path("approval_version").asInt()).isEqualTo(2);
    assertThat(outbox.path("binding_digest").asText()).isEqualTo(bindingDigest);
    assertThat(outbox.path("decision").asText()).isEqualTo("allow_once");

    var resumedPayload = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.resumed", resumedPayload,
        sha256(resumedPayload)))).isTrue();
    var executionPayload = """
        {"execution":{"call":{"id":"call_shell","name":"shell","arguments":{"command":"cargo test"}},"effect":"unknown","sandbox":"kata","binding_digest":"%s"}}
        """.formatted(bindingDigest).strip();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 3,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.execution.started",
        executionPayload, sha256(executionPayload)))).isTrue();
    var toolResultPayload = """
        {"tool_call_id":"call_shell","binding_digest":"%s","content":{"stdout":"ok"},"is_error":false}
        """.formatted(bindingDigest).strip();
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 4,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "tool.result", toolResultPayload,
        sha256(toolResultPayload)))).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select status,last_sequence from runs where tenant_id = ? and id = ?
        """, command.tenantId(), command.runId()))
        .containsEntry("status", "running")
        .containsEntry("last_sequence", 4L);

    assertThatThrownBy(() -> approvals.decide(
        fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
        approvalId, 1, ApprovalDecision.ALLOW_ONCE, null, "user-42", decidedAt)))
        .isInstanceOf(ApprovalConflict.class);
    assertThatThrownBy(() -> approvals.decide(
        UUID.randomUUID(), UUID.randomUUID(), new DecideApprovalCommand(
        approvalId, 1, ApprovalDecision.DENY, null, "attacker", decidedAt)))
        .isInstanceOf(ApprovalNotFound.class);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'tool.approval.decided'
        """, Integer.class, command.tenantId(), command.runId())).isOne();
  }

  @Test
  void pendingApprovalListIsApplicationScopedAndContainsTheReviewedToolBinding() throws Exception {
    var fixture = fixture("scheduler-approval-list");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var approvalId = UUID.randomUUID();
    var bindingDigest = "d".repeat(64);
    var payload = """
        {"approval":{"approval_id":"%s","execution":{"call":{"id":"call_readme","name":"workspace.read_text","arguments":{"path":"README.md"}},"effect":"pure","sandbox":"trusted_native","binding_digest":"%s"}},"status":"waiting_approval"}
        """.formatted(approvalId, bindingDigest).strip();
    fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.parse("2026-08-02T04:00:00Z"), UUID.randomUUID(),
        "approval.required", payload, sha256(payload)));
    var approvals = new JdbcApprovalRepository(
        fixture.jdbc(), new TransactionTemplate(
            new DataSourceTransactionManager(fixture.jdbc().getDataSource())));

    assertThat(approvals.findPending(
        fixture.tenantId(), UUID.randomUUID(), 25)).isEmpty();
    assertThat(approvals.findPending(
        UUID.randomUUID(), fixture.applicationId(), 25)).isEmpty();
    assertThat(approvals.findPending(
        fixture.tenantId(), fixture.applicationId(), 25))
        .singleElement()
        .satisfies(item -> {
          assertThat(item.id()).isEqualTo(approvalId);
          assertThat(item.runId()).isEqualTo(command.runId());
          assertThat(item.workspaceName()).isEqualTo("Workspace");
          assertThat(item.agentName()).isEqualTo("Agent");
          assertThat(item.toolName()).isEqualTo("workspace.read_text");
          assertThat(item.toolCallId()).isEqualTo("call_readme");
          assertThat(item.effect()).isEqualTo("pure");
          assertThat(item.sandbox()).isEqualTo("trusted_native");
          assertThat(item.bindingDigest()).isEqualTo(bindingDigest);
          assertThat(item.arguments().path("path").asText()).isEqualTo("README.md");
          assertThat(item.status()).isEqualTo(com.agentplatform.control.approval.ApprovalStatus.PENDING);
        });

    approvals.decide(fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
        approvalId, 1, ApprovalDecision.DENY, null, "reviewer-7", Instant.now()));
    assertThat(approvals.findPending(
        fixture.tenantId(), fixture.applicationId(), 25)).isEmpty();
  }

  @Test
  void replaySafeSessionGrantAutoApprovesOnlyTheSameArgumentsAndPolicySnapshot() throws Exception {
    var fixture = fixture("scheduler-session-approval");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var firstApprovalId = UUID.randomUUID();
    var firstPayload = approvalPayload(
        firstApprovalId, "call_read_1", "README.md", "a".repeat(64));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "approval.required", firstPayload, sha256(firstPayload)))).isTrue();
    var approvals = new JdbcApprovalRepository(
        fixture.jdbc(), new TransactionTemplate(
            new DataSourceTransactionManager(fixture.jdbc().getDataSource())));

    approvals.decide(fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
        firstApprovalId, 1, ApprovalDecision.ALLOW_SESSION, "same read in this session",
        "reviewer-7", Instant.now()));

    assertThat(fixture.jdbc().queryForMap("""
        select application_id,session_id,workspace_id,agent_version_id,tool_name,effect,
               source_approval_id
          from session_tool_grants where tenant_id = ?
        """, fixture.tenantId()))
        .containsEntry("application_id", fixture.applicationId())
        .containsEntry("session_id", command.sessionId())
        .containsEntry("workspace_id", fixture.workspaceId())
        .containsEntry("agent_version_id", fixture.agentVersionId())
        .containsEntry("tool_name", "workspace.read_text")
        .containsEntry("effect", "pure")
        .containsEntry("source_approval_id", firstApprovalId);

    var resumed = "{\"status\":\"running\"}";
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "run.resumed", resumed, sha256(resumed)))).isTrue();
    var repeatedApprovalId = UUID.randomUUID();
    var repeatedPayload = approvalPayload(
        repeatedApprovalId, "call_read_2", "README.md", "b".repeat(64));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 3,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "approval.required", repeatedPayload, sha256(repeatedPayload)))).isTrue();

    assertThat(fixture.jdbc().queryForMap("""
        select status,version from approvals where tenant_id = ? and id = ?
        """, fixture.tenantId(), repeatedApprovalId))
        .containsEntry("status", "approved")
        .containsEntry("version", 2);
    assertThat(fixture.jdbc().queryForObject("""
        select payload->>'decision' from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'tool.approval.decided'
           and payload->>'approval_id' = ?
        """, String.class, fixture.tenantId(), command.runId(), repeatedApprovalId.toString()))
        .isEqualTo("allow_once");

    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 4,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "run.resumed", resumed, sha256(resumed)))).isTrue();
    var changedApprovalId = UUID.randomUUID();
    var changedPayload = approvalPayload(
        changedApprovalId, "call_read_3", "SECURITY.md", "c".repeat(64));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 5,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "approval.required", changedPayload, sha256(changedPayload)))).isTrue();
    assertThat(fixture.jdbc().queryForObject("""
        select status from approvals where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), changedApprovalId)).isEqualTo("pending");

    approvals.decide(fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
        changedApprovalId, 1, ApprovalDecision.DENY, null, "reviewer-7", Instant.now()));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 6,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "run.resumed", resumed, sha256(resumed)))).isTrue();
    var changedPolicyApprovalId = UUID.randomUUID();
    var changedPolicyPayload = approvalPayload(
        changedPolicyApprovalId, "call_read_4", "README.md", "e".repeat(64), "pure",
        "e".repeat(64));
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 7,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "approval.required", changedPolicyPayload, sha256(changedPolicyPayload)))).isTrue();
    assertThat(fixture.jdbc().queryForObject("""
        select status from approvals where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), changedPolicyApprovalId)).isEqualTo("pending");
  }

  @Test
  void sessionGrantRejectsToolsWithUnknownSideEffects() throws Exception {
    var fixture = fixture("scheduler-session-approval-unknown-effect");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var approvalId = UUID.randomUUID();
    var payload = approvalPayload(
        approvalId, "call_unknown", "README.md", "f".repeat(64), "unknown");
    assertThat(fixture.scheduler().recordRunEvent(new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(),
        "approval.required", payload, sha256(payload)))).isTrue();
    var approvals = new JdbcApprovalRepository(
        fixture.jdbc(), new TransactionTemplate(
            new DataSourceTransactionManager(fixture.jdbc().getDataSource())));

    assertThatThrownBy(() -> approvals.decide(
        fixture.tenantId(), fixture.applicationId(), new DecideApprovalCommand(
            approvalId, 1, ApprovalDecision.ALLOW_SESSION, null, "reviewer-7", Instant.now())))
        .isInstanceOf(ApprovalDecisionNotAllowed.class);
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from session_tool_grants where tenant_id = ?
        """, Integer.class, fixture.tenantId())).isZero();
    assertThat(fixture.jdbc().queryForObject("""
        select status from approvals where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), approvalId)).isEqualTo("pending");
  }

  @Test
  void terminalEventIsPersistedBeforeAssignmentCapacityAndWorkspaceAreReleased() throws Exception {
    var fixture = fixture("scheduler-terminal-event");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var started = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 1,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.started",
        "{\"status\":\"running\"}",
        "409443a6ee5aa296dccd6c0d193e214568daa0053b66155fba8adca995b7823d");
    assertThat(fixture.scheduler().recordRunEvent(started)).isTrue();
    var terminal = new RunEventMessage(
        UUID.randomUUID(), 1, command.tenantId(), command.sessionId(), command.runId(), 2,
        command.attemptId(), Instant.now(), UUID.randomUUID(), "run.succeeded",
        "{\"status\":\"succeeded\"}",
        "f24874c0a20560e8a002a58d258bae2e4d0b92a9c69139de3adada7d3ef9b1d4");

    assertThat(fixture.scheduler().recordRunEvent(terminal)).isTrue();
    assertThat(fixture.scheduler().recordRunEvent(terminal)).isTrue();
    assertThat(fixture.jdbc().queryForMap("""
        select status,current_attempt_id,finished_at,last_sequence
          from runs where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.runId()))
        .containsEntry("status", "succeeded")
        .containsEntry("current_attempt_id", null)
        .containsEntry("last_sequence", 2L);
    assertThat(fixture.jdbc().queryForObject("""
        select finished_at is not null from runs where tenant_id = ? and id = ?
        """, Boolean.class, fixture.tenantId(), fixture.runId())).isTrue();
    assertThat(fixture.jdbc().queryForObject("""
        select state from run_dispatches
         where tenant_id = ? and run_id = ? and attempt_id = ?
        """, String.class, fixture.tenantId(), fixture.runId(), command.attemptId()))
        .isEqualTo("finished");
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from workspace_leases where tenant_id = ? and workspace_id = ?
        """, Integer.class, fixture.tenantId(), fixture.workspaceId())).isOne();
    assertThat(fixture.jdbc().queryForObject("""
        select expires_at <= clock_timestamp() from workspace_leases
         where tenant_id = ? and workspace_id = ?
        """, Boolean.class, fixture.tenantId(), fixture.workspaceId())).isTrue();
    assertThat(fixture.jdbc().queryForObject(
        "select active_runs from runtime_workers where id = ?", Integer.class, workerId)).isZero();
    assertThat(fixture.jdbc().queryForObject("""
        select state from workspaces where tenant_id = ? and id = ?
        """, String.class, fixture.tenantId(), fixture.workspaceId())).isEqualTo("ready");
  }

  @Test
  void runningRunCancellationIsTargetedToTheCurrentWorkerAndAttemptOnce() throws Exception {
    var fixture = fixture("scheduler-cancel-running");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    fixture.scheduler().recordAcceptance(new ExecutionAcceptedMessage(
        1, UUID.randomUUID(), command.tenantId(), command.runId(), command.attemptId(),
        command.workerId(), Instant.now()));
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var runs = new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));

    assertThat(runs.requestCancellation(
        fixture.tenantId(), fixture.runId(), Instant.now())).isEqualTo(RunStatus.RUNNING);
    assertThat(runs.requestCancellation(
        fixture.tenantId(), fixture.runId(), Instant.now())).isEqualTo(RunStatus.RUNNING);

    var payload = new com.fasterxml.jackson.databind.ObjectMapper().readTree(
        fixture.jdbc().queryForObject("""
            select payload::text from outbox_events
             where tenant_id = ? and aggregate_id = ?
               and event_type = 'run.cancellation.requested'
            """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("worker_id").asText()).isEqualTo(workerId.toString());
    assertThat(payload.path("worker_incarnation_id").asText())
        .isEqualTo(command.workerIncarnationId().toString());
    assertThat(payload.path("schema_version").asInt()).isEqualTo(2);
    assertThat(payload.path("attempt_id").asText()).isEqualTo(command.attemptId().toString());
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from outbox_events
         where tenant_id = ? and aggregate_id = ? and event_type = 'run.cancellation.requested'
        """, Integer.class, fixture.tenantId(), fixture.runId())).isOne();
  }

  @Test
  void cancellationDuringDispatchTargetsThePendingAttemptInsteadOfStartingAnotherOne() throws Exception {
    var fixture = fixture("scheduler-cancel-pending");
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0,
        List.of(), "0.1.0"), Duration.ofSeconds(30));
    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var runs = new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));

    assertThat(runs.requestCancellation(
        fixture.tenantId(), fixture.runId(), Instant.now())).isEqualTo(RunStatus.QUEUED);

    var payload = new com.fasterxml.jackson.databind.ObjectMapper().readTree(
        fixture.jdbc().queryForObject("""
            select payload::text from outbox_events
             where tenant_id = ? and aggregate_id = ?
               and event_type = 'run.cancellation.requested'
            """, String.class, fixture.tenantId(), fixture.runId()));
    assertThat(payload.path("attempt_id").asText()).isEqualTo(command.attemptId().toString());
    assertThat(fixture.jdbc().queryForObject("""
        select count(*) from run_dispatches
         where tenant_id = ? and run_id = ? and state = 'requested'
        """, Integer.class, fixture.tenantId(), fixture.runId())).isOne();
  }

  private void expireAssignment(Fixture fixture, UUID workerId) {
    fixture.jdbc().update("""
        update run_dispatches
           set lease_expires_at = now() - interval '1 second',
               workload_identity_expires_at = now() - interval '1 second'
         where tenant_id = ? and run_id = ?
        """, fixture.tenantId(), fixture.runId());
    fixture.jdbc().update("""
        update workspace_leases set expires_at = now() - interval '1 second'
         where tenant_id = ? and workspace_id = ?
        """, fixture.tenantId(), fixture.workspaceId());
    fixture.jdbc().update("""
        update runtime_workers set last_heartbeat = now() - interval '1 minute'
         where id = ? and current_incarnation_id in (
           select d.worker_incarnation_id from run_dispatches d
            where d.tenant_id = ? and d.run_id = ? and d.worker_id = ?)
        """, workerId, fixture.tenantId(), fixture.runId(), workerId);
    fixture.jdbc().update("""
        update runtime_worker_incarnations
           set last_heartbeat = now() - interval '1 minute',
               last_heartbeat_received_at = now() - interval '1 minute'
         where (worker_id,incarnation_id) in (
           select d.worker_id,d.worker_incarnation_id from run_dispatches d
            where d.tenant_id = ? and d.run_id = ? and d.worker_id = ?)
        """, fixture.tenantId(), fixture.runId(), workerId);
  }

  /**
   * A registered MCP server has to actually arrive in the command and in the
   * outbox payload.
   *
   * <p>The suite passing after the projection was added only proves the columns
   * line up; it does not prove anything reached the Worker. This is the
   * assertion that would fail if the projection returned an empty array
   * everywhere, which is exactly what a missing join would produce.
   */
  @Test
  void dispatchCarriesDelegatedMcpServersSealedIntoTheCommandAndOutbox() throws Exception {
    var fixture = fixture("scheduler-mcp-servers");
    fixture.jdbc().update("""
        insert into mcp_servers (
          tenant_id,id,application_id,name,endpoint,credential_envelope)
        values (?,?,?,'search','https://mcp.example.com/rpc',
                '{"schema_version":1,"key_id":"k","algorithm":"a",
                  "encrypted_key":"e","nonce":"n","ciphertext":"c"}'::jsonb)
        """, fixture.tenantId(), UUID.randomUUID(), fixture.applicationId());
    // A second server nobody delegates, so "carried" is distinguishable from
    // "everything registered was carried".
    fixture.jdbc().update("""
        insert into mcp_servers (tenant_id,id,application_id,name,endpoint)
        values (?,?,?,'undelegated','https://other.example.com/rpc')
        """, fixture.tenantId(), UUID.randomUUID(), fixture.applicationId());
    fixture.jdbc().update("""
        update agent_versions
           set spec = '{"instructions":"Search before answering.",
                        "delegated_scopes":["tool:mcp:search"]}'::jsonb
         where tenant_id = ? and id = ?
        """, fixture.tenantId(), fixture.agentVersionId());
    var workerId = UUID.randomUUID();
    fixture.scheduler().recordHeartbeat(new WorkerHeartbeatMessage(
        1, UUID.randomUUID(), workerId, Instant.now(), List.of("cloud"), 4, 0, "0.1.0"));

    var command = fixture.scheduler().schedule(
        fixture.tenantId(), fixture.runId(), Duration.ofSeconds(30), Duration.ofSeconds(15))
        .command().orElseThrow();

    assertThat(command.mcpServers()).hasSize(1);
    var server = command.mcpServers().getFirst();
    assertThat(server.name()).isEqualTo("search");
    assertThat(server.endpoint()).isEqualTo("https://mcp.example.com/rpc");
    // Sealed and base64: the Worker gets something opaque, and it must not be
    // wrapped at 76 characters the way PostgreSQL's own encode() would leave it.
    assertThat(server.credentialEnvelopeBase64()).isNotBlank().doesNotContain("\n");
    assertThat(new String(java.util.Base64.getDecoder().decode(
        server.credentialEnvelopeBase64()), java.nio.charset.StandardCharsets.UTF_8))
        .contains("ciphertext");

    var payload = new ObjectMapper().readTree(fixture.jdbc().queryForObject("""
        select payload::text from outbox_events
         where tenant_id = ? and event_type = 'run.execution.requested'
         order by created_at desc limit 1
        """, String.class, fixture.tenantId()));
    var carried = payload.path("mcp_servers");
    assertThat(carried.size()).isEqualTo(1);
    assertThat(carried.get(0).path("name").asText()).isEqualTo("search");
    assertThat(carried.get(0).path("credential_envelope_base64").asText()).isNotBlank();
  }

  private Fixture fixture(String idempotencyKey) throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    try (Connection connection = DriverManager.getConnection(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
        var statement = connection.createStatement()) {
      statement.execute("update runtime_workers set last_heartbeat = now() - interval '1 hour'");
      statement.execute("""
          update runtime_worker_incarnations
             set last_heartbeat = now() - interval '1 hour',
                 last_heartbeat_received_at = now() - interval '1 hour'
          """);
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
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);
    var transactions = new TransactionTemplate(new DataSourceTransactionManager(dataSource));
    var runs = new JdbcRunRepository(jdbc, transactions);
    runs.save(applicationId, new Run(
        runId, tenantId, sessionId, agentVersionId, workspaceId, modelPolicyId, idempotencyKey, "hello",
        RunStatus.QUEUED, 1000, 100, 60, Instant.now()));
    return new Fixture(
        tenantId, applicationId, workspaceId, agentVersionId, modelPolicyId, runId, jdbc,
        new JdbcSchedulerRepository(jdbc, transactions, tokenIssuer()));
  }

  private Ed25519WorkloadTokenIssuer tokenIssuer() throws Exception {
    var keys = KeyPairGenerator.getInstance("Ed25519").generateKeyPair();
    return new Ed25519WorkloadTokenIssuer(keys.getPrivate(), new ObjectMapper(), Clock.systemUTC());
  }

  private String sha256(String value) throws Exception {
    return HexFormat.of().formatHex(
        MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8)));
  }

  private String approvalPayload(
      UUID approvalId, String toolCallId, String path, String bindingDigest) throws Exception {
    return approvalPayload(approvalId, toolCallId, path, bindingDigest, "pure");
  }

  private String approvalPayload(
      UUID approvalId, String toolCallId, String path, String bindingDigest, String effect)
      throws Exception {
    return approvalPayload(
        approvalId, toolCallId, path, bindingDigest, effect, "d".repeat(64));
  }

  private String approvalPayload(
      UUID approvalId,
      String toolCallId,
      String path,
      String bindingDigest,
      String effect,
      String implementationDigest) throws Exception {
    var policySnapshot = """
        {"approval":"ask","effect":"%s","implementation_digest":"%s","required_scopes":["workspace:read"],"sandbox":"trusted_native","tool_name":"workspace.read_text"}
        """.formatted(effect, implementationDigest).strip();
    var policyDigest = sha256(policySnapshot);
    var arguments = "{\"path\":\"%s\"}".formatted(path);
    var scope = """
        {"arguments":%s,"policy_snapshot":%s,"tool_name":"workspace.read_text"}
        """.formatted(arguments, policySnapshot).strip();
    return """
        {"approval":{"approval_id":"%s","execution":{"call":{"id":"%s","name":"workspace.read_text","arguments":%s},"effect":"%s","sandbox":"trusted_native","binding_digest":"%s"},"policy_snapshot":%s,"policy_digest":"%s","session_scope_digest":"%s"},"status":"waiting_approval"}
        """.formatted(
            approvalId, toolCallId, arguments, effect, bindingDigest, policySnapshot,
            policyDigest, sha256(scope)).strip();
  }

  private record Fixture(
      UUID tenantId,
      UUID applicationId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID modelPolicyId,
      UUID runId,
      JdbcTemplate jdbc,
      JdbcSchedulerRepository scheduler) {}
}
