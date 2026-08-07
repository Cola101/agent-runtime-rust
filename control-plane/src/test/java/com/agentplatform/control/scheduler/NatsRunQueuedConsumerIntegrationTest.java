package com.agentplatform.control.scheduler;

import static org.assertj.core.api.Assertions.assertThat;

import com.agentplatform.control.outbox.NatsJetStreamMessageBus;
import com.agentplatform.control.outbox.OutboxMessage;
import com.agentplatform.control.outbox.OutboxSubjects;
import com.agentplatform.control.outbox.RuntimeNatsTopology;
import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import java.time.Duration;
import java.time.Instant;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Base64;
import java.util.HexFormat;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.springframework.boot.test.context.runner.ApplicationContextRunner;

class NatsRunQueuedConsumerIntegrationTest {
  @Test
  void successfulSchedulingExplicitlyAcknowledgesQueuedRun() throws Exception {
    purgeControlStream();
    var tenantId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    publishRunQueued(tenantId, runId);
    var received = new AtomicReference<QueuedRun>();

    try (var consumer = NatsRunQueuedConsumer.connect(
        NativeIntegrationEnvironment.natsSettings(), "scheduler-success", queued -> {
          received.set(queued);
          return ScheduleResult.withoutCommand(ScheduleStatus.DISPATCHED);
        }, Duration.ofMillis(25))) {
      assertThat(consumer.poll(Duration.ofSeconds(2))).isEqualTo(SchedulerPollResult.ACKED);
    }

    assertThat(received.get()).isEqualTo(new QueuedRun(tenantId, runId));
    try (var observer = NativeIntegrationEnvironment.connectNats()) {
      var info = observer.jetStreamManagement().getConsumerInfo(
          NatsJetStreamMessageBus.STREAM_NAME, "scheduler-success");
      assertThat(info.getNumAckPending()).isZero();
    }
  }

  @Test
  void lackOfCapacityNaksForLaterRedelivery() throws Exception {
    purgeControlStream();
    publishRunQueued(UUID.randomUUID(), UUID.randomUUID());

    try (var consumer = NatsRunQueuedConsumer.connect(
        NativeIntegrationEnvironment.natsSettings(), "scheduler-retry", queued ->
            ScheduleResult.withoutCommand(ScheduleStatus.RETRY_NO_CAPACITY),
        Duration.ofMillis(25))) {
      assertThat(consumer.poll(Duration.ofSeconds(2))).isEqualTo(SchedulerPollResult.RETRY_SCHEDULED);
      Thread.sleep(50);
      assertThat(consumer.poll(Duration.ofSeconds(2))).isEqualTo(SchedulerPollResult.RETRY_SCHEDULED);
    }
  }

  @Test
  void malformedQueuedRunIsTerminatedInsteadOfRedeliveredForever() throws Exception {
    purgeControlStream();
    var messageId = UUID.randomUUID();
    try (var bus = NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings())) {
      bus.publish(new OutboxMessage(
          UUID.randomUUID(), messageId, "run", UUID.randomUUID(), "run.queued", "{}",
          Instant.now(), 1, UUID.randomUUID()));
    }

    try (var consumer = NatsRunQueuedConsumer.connect(
        NativeIntegrationEnvironment.natsSettings(), "scheduler-poison", queued -> {
          throw new AssertionError("malformed command must not reach scheduler");
        }, Duration.ofMillis(25))) {
      assertThat(consumer.poll(Duration.ofSeconds(2))).isEqualTo(SchedulerPollResult.TERMINATED);
      assertThat(consumer.poll(Duration.ofMillis(100))).isEqualTo(SchedulerPollResult.IDLE);
    }
  }

  @Test
  void enabledSchedulerConfigurationCreatesIndependentConsumerLoop() {
    RunQueuedHandler handler = queued ->
        ScheduleResult.withoutCommand(ScheduleStatus.RETRY_NO_CAPACITY);
    WorkerEventHandler workerEvents = new WorkerEventHandler() {
      @Override
      public void onHeartbeat(WorkerHeartbeatMessage heartbeat) {}

      @Override
      public boolean onAccepted(ExecutionAcceptedMessage accepted) {
        return true;
      }
    };
    DispatchReconciler reconciler = () -> new ReconcileResult(0, 0);
    RecoveryMetricsSource metrics = objective -> new RecoverySloSnapshot(0, 0, 0, 0, 0);

    new ApplicationContextRunner()
        .withUserConfiguration(SchedulerConfiguration.class)
        .withBean(RunQueuedHandler.class, () -> handler)
        .withBean(WorkerEventHandler.class, () -> workerEvents)
        .withBean(DispatchReconciler.class, () -> reconciler)
        .withBean(RecoveryMetricsSource.class, () -> metrics)
        .withPropertyValues(
            NativeIntegrationEnvironment.natsSecurityProperties("agent.runtime.scheduler"))
        .withPropertyValues(
            "agent.runtime.scheduler.enabled=true",
            "agent.runtime.scheduler.durable-name=scheduler-config-test",
            "agent.runtime.scheduler.retry-delay=50ms",
            "agent.runtime.scheduler.poll-timeout=100ms")
        .run(context -> {
          assertThat(context).hasSingleBean(NatsRunQueuedConsumer.class);
          assertThat(context).hasSingleBean(ScheduledRunQueuedConsumer.class);
          assertThat(context).hasSingleBean(NatsWorkerEventConsumer.class);
          assertThat(context).hasSingleBean(ScheduledWorkerEventConsumer.class);
          assertThat(context).hasSingleBean(ScheduledDispatchReconciler.class);
          assertThat(context).hasSingleBean(RecoveryMetricsCollector.class);
        });
  }

  @Test
  void workerHeartbeatAndAcceptanceAreConsumedFromDurableWorkerEventStream() throws Exception {
    var workerId = UUID.randomUUID();
    var tenantId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var attemptId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var heartbeatReceived = new AtomicReference<WorkerHeartbeatMessage>();
    var acceptanceReceived = new AtomicReference<ExecutionAcceptedMessage>();
    var runEventReceived = new AtomicReference<RunEventMessage>();
    var checkpointReceived = new AtomicReference<RunCheckpointMessage>();
    var steeringOutcomeReceived = new AtomicReference<RunSteeringOutcomeMessage>();
    WorkerEventHandler handler = new WorkerEventHandler() {
      @Override
      public void onHeartbeat(WorkerHeartbeatMessage heartbeat) {
        if (workerId.equals(heartbeat.workerId())
            && incarnationId.equals(heartbeat.incarnationId())) {
          heartbeatReceived.set(heartbeat);
        }
      }

      @Override
      public boolean onAccepted(ExecutionAcceptedMessage accepted) {
        if (runId.equals(accepted.runId()) && attemptId.equals(accepted.attemptId())) {
          acceptanceReceived.set(accepted);
        }
        return true;
      }

      @Override
      public boolean onRunEvent(RunEventMessage event) {
        if (runId.equals(event.runId()) && attemptId.equals(event.attemptId())) {
          runEventReceived.set(event);
        }
        return true;
      }

      @Override
      public boolean onCheckpoint(RunCheckpointMessage checkpoint) {
        if (runId.equals(checkpoint.runId()) && attemptId.equals(checkpoint.attemptId())) {
          checkpointReceived.set(checkpoint);
        }
        return true;
      }

      @Override
      public void onSteeringOutcome(RunSteeringOutcomeMessage outcome) {
        if (runId.equals(outcome.runId()) && attemptId.equals(outcome.attemptId())) {
          steeringOutcomeReceived.set(outcome);
        }
      }
    };
    ensureNatsTopology();
    try (var connection = NativeIntegrationEnvironment.connectWorkerNats()) {
      var jetstream = connection.jetStream();
      jetstream.publish("runtime.worker.heartbeat.v2", """
          {"schema_version":2,"message_id":"%s","worker_id":"%s",
           "incarnation_id":"%s",
           "occurred_at":"%s","placements":["cloud"],"capacity":4,
           "active_runs":1,"active_assignments":[{
             "tenant_id":"%s","run_id":"%s","attempt_id":"%s",
             "workspace_id":"%s","owner_epoch":3,"fencing_token":"%s"
           }],"runtime_version":"0.1.0"}
          """.formatted(UUID.randomUUID(), workerId, incarnationId, Instant.now(), tenantId,
          runId, attemptId,
          UUID.randomUUID(), UUID.randomUUID()).getBytes());
      jetstream.publish("runtime.worker.execution.accepted.v2", """
          {"schema_version":2,"message_id":"%s","tenant_id":"%s","run_id":"%s",
           "attempt_id":"%s","worker_id":"%s","worker_incarnation_id":"%s",
           "accepted_at":"%s"}
          """.formatted(UUID.randomUUID(), tenantId, runId, attemptId, workerId,
          incarnationId, Instant.now())
          .getBytes());
      jetstream.publish("runtime.worker.run.event.v1", """
          {"event_id":"%s","schema_version":1,"tenant_id":"%s",
           "session_id":"%s","run_id":"%s","sequence":1,"attempt_id":"%s",
           "timestamp":"%s","trace_id":"%s","type":"run.started",
           "payload":{"status":"running"},
           "digest":"409443a6ee5aa296dccd6c0d193e214568daa0053b66155fba8adca995b7823d"}
          """.formatted(UUID.randomUUID(), tenantId, UUID.randomUUID(), runId, attemptId,
          Instant.now(), UUID.randomUUID()).getBytes());
      var checkpointPayload = "{}".getBytes(StandardCharsets.UTF_8);
      jetstream.publish("runtime.worker.run.checkpoint.v1", """
          {"schema_version":1,"message_id":"%s","tenant_id":"%s",
           "run_id":"%s","session_id":"%s","attempt_id":"%s",
           "owner_epoch":3,"fencing_token":"%s","sequence":1,"status":"running",
           "kernel_digest":"%s","tool_catalog_digest":"%s",
           "payload_base64":"%s","payload_digest":"%s","created_at":"%s"}
          """.formatted(UUID.randomUUID(), tenantId, runId, UUID.randomUUID(), attemptId,
          UUID.randomUUID(), "a".repeat(64), "b".repeat(64),
          Base64.getEncoder().encodeToString(checkpointPayload), sha256(checkpointPayload),
          Instant.now()).getBytes());
      jetstream.publish("runtime.worker.run.steering.outcome.v1", """
          {"schema_version":1,"message_id":"%s","steering_id":"%s",
           "tenant_id":"%s","run_id":"%s","attempt_id":"%s",
           "worker_id":"%s","worker_incarnation_id":"%s",
           "input_digest":"%s","outcome":"rejected","reason":"expired",
           "occurred_at":"%s"}
          """.formatted(UUID.randomUUID(), UUID.randomUUID(), tenantId, runId, attemptId,
          workerId, incarnationId, "c".repeat(64), Instant.now()).getBytes());
    }

    try (var consumer = NatsWorkerEventConsumer.connect(
        NativeIntegrationEnvironment.natsSettings(), "worker-events-test-" + workerId, handler)) {
      for (var poll = 0; poll < 20
          && (heartbeatReceived.get() == null || acceptanceReceived.get() == null
              || runEventReceived.get() == null || checkpointReceived.get() == null
              || steeringOutcomeReceived.get() == null); poll++) {
        assertThat(consumer.poll(Duration.ofSeconds(2)))
            .isNotEqualTo(WorkerEventPollResult.TERMINATED);
      }
    }

    assertThat(heartbeatReceived.get().workerId()).isEqualTo(workerId);
    assertThat(heartbeatReceived.get().incarnationId()).isEqualTo(incarnationId);
    assertThat(heartbeatReceived.get().placements()).isEqualTo(List.of("cloud"));
    assertThat(heartbeatReceived.get().acceptingWork()).isTrue();
    assertThat(heartbeatReceived.get().drainingSince()).isNull();
    assertThat(heartbeatReceived.get().drainDeadline()).isNull();
    assertThat(heartbeatReceived.get().activeAssignments()).hasSize(1);
    assertThat(heartbeatReceived.get().activeAssignments().getFirst().attemptId())
        .isEqualTo(attemptId);
    assertThat(acceptanceReceived.get().runId()).isEqualTo(runId);
    assertThat(acceptanceReceived.get().attemptId()).isEqualTo(attemptId);
    assertThat(acceptanceReceived.get().workerIncarnationId()).isEqualTo(incarnationId);
    assertThat(runEventReceived.get().runId()).isEqualTo(runId);
    assertThat(runEventReceived.get().type()).isEqualTo("run.started");
    assertThat(checkpointReceived.get().attemptId()).isEqualTo(attemptId);
    assertThat(new String(checkpointReceived.get().payload(), StandardCharsets.UTF_8))
        .isEqualTo("{}");
    assertThat(steeringOutcomeReceived.get().runId()).isEqualTo(runId);
    assertThat(steeringOutcomeReceived.get().reason()).isEqualTo("expired");
  }

  @Test
  void checkpointThatArrivesBeforeItsRunEventIsNakedForOrderedRetry() throws Exception {
    var attempts = new AtomicInteger();
    var tenantId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var attemptId = UUID.randomUUID();
    var checkpointPayload = "{}".getBytes(StandardCharsets.UTF_8);
    WorkerEventHandler handler = new WorkerEventHandler() {
      @Override
      public void onHeartbeat(WorkerHeartbeatMessage heartbeat) {}

      @Override
      public boolean onAccepted(ExecutionAcceptedMessage accepted) {
        return true;
      }

      @Override
      public boolean onCheckpoint(RunCheckpointMessage checkpoint) {
        return attempts.incrementAndGet() > 1;
      }
    };
    ensureNatsTopology();
    try (var connection = NativeIntegrationEnvironment.connectWorkerNats()) {
      connection.jetStream().publish("runtime.worker.run.checkpoint.v1", """
          {"schema_version":1,"message_id":"%s","tenant_id":"%s",
           "run_id":"%s","session_id":"%s","attempt_id":"%s",
           "owner_epoch":1,"fencing_token":"%s","sequence":1,"status":"running",
           "kernel_digest":"%s","tool_catalog_digest":"%s",
           "payload_base64":"%s","payload_digest":"%s","created_at":"%s"}
          """.formatted(UUID.randomUUID(), tenantId, runId, UUID.randomUUID(), attemptId,
          UUID.randomUUID(), "a".repeat(64), "b".repeat(64),
          Base64.getEncoder().encodeToString(checkpointPayload), sha256(checkpointPayload),
          Instant.now()).getBytes());
    }

    try (var consumer = NatsWorkerEventConsumer.connect(
        NativeIntegrationEnvironment.natsSettings(), "checkpoint-retry-" + attemptId, handler)) {
      WorkerEventPollResult firstCheckpointResult = WorkerEventPollResult.IDLE;
      for (var poll = 0; poll < 20; poll++) {
        firstCheckpointResult = consumer.poll(Duration.ofMillis(500));
        if (firstCheckpointResult == WorkerEventPollResult.RETRY_SCHEDULED) {
          break;
        }
      }
      assertThat(firstCheckpointResult).isEqualTo(WorkerEventPollResult.RETRY_SCHEDULED);
      Thread.sleep(1100);
      WorkerEventPollResult retriedCheckpointResult = WorkerEventPollResult.IDLE;
      for (var poll = 0; poll < 20; poll++) {
        retriedCheckpointResult = consumer.poll(Duration.ofMillis(500));
        if (retriedCheckpointResult == WorkerEventPollResult.CHECKPOINT_RECORDED) {
          break;
        }
      }
      assertThat(retriedCheckpointResult).isEqualTo(WorkerEventPollResult.CHECKPOINT_RECORDED);
    }
  }

  @Test
  void v2HeartbeatWithProcessIncarnationIsAccepted() throws Exception {
    var recorded = new AtomicInteger();
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    WorkerEventHandler handler = new WorkerEventHandler() {
      @Override
      public void onHeartbeat(WorkerHeartbeatMessage heartbeat) {
        if (workerId.equals(heartbeat.workerId())) {
          recorded.incrementAndGet();
        }
      }

      @Override
      public boolean onAccepted(ExecutionAcceptedMessage accepted) {
        return true;
      }
    };
    ensureNatsTopology();
    try (var connection = NativeIntegrationEnvironment.connectWorkerNats()) {
      connection.jetStream().publish("runtime.worker.heartbeat.v2", """
          {"schema_version":2,"message_id":"%s","worker_id":"%s",
           "incarnation_id":"%s","occurred_at":"%s","placements":["cloud"],
           "capacity":4,"active_runs":0,"active_assignments":[],
           "runtime_version":"0.1.0"}
          """.formatted(UUID.randomUUID(), workerId, incarnationId, Instant.now()).getBytes());
    }

    try (var consumer = NatsWorkerEventConsumer.connect(
        NativeIntegrationEnvironment.natsSettings(), "heartbeat-v2-" + incarnationId, handler)) {
      WorkerEventPollResult result = WorkerEventPollResult.IDLE;
      for (var poll = 0; poll < 20; poll++) {
        result = consumer.poll(Duration.ofMillis(500));
        if (recorded.get() == 1 || result == WorkerEventPollResult.TERMINATED) {
          break;
        }
      }
      assertThat(result).isEqualTo(WorkerEventPollResult.HEARTBEAT_RECORDED);
      assertThat(recorded).hasValue(1);
    }
  }

  private void publishRunQueued(UUID tenantId, UUID runId) {
    var messageId = UUID.randomUUID();
    var payload = """
        {
          "schema_version":1,
          "message_id":"%s",
          "tenant_id":"%s",
          "run_id":"%s"
        }
        """.formatted(messageId, tenantId, runId);
    try (var bus = NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings())) {
      bus.publish(new OutboxMessage(
          tenantId, messageId, "run", runId, "run.queued", payload,
          Instant.now(), 1, UUID.randomUUID()));
    } catch (InterruptedException exception) {
      Thread.currentThread().interrupt();
      throw new IllegalStateException(exception);
    }
  }

  private void purgeControlStream() throws Exception {
    try (var bus = NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings())) {
      // Creating the bus establishes the topology before the test purges prior messages.
    }
    try (var connection = NativeIntegrationEnvironment.connectNats()) {
      connection.jetStreamManagement().purgeStream(NatsJetStreamMessageBus.STREAM_NAME);
    }
  }

  private void ensureNatsTopology() throws Exception {
    try (var connection = NativeIntegrationEnvironment.connectNats()) {
      RuntimeNatsTopology.ensure(connection);
    }
  }

  private String sha256(byte[] value) throws Exception {
    return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(value));
  }
}
