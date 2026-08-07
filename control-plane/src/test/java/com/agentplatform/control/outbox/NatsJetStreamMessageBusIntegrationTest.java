package com.agentplatform.control.outbox;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import java.nio.charset.StandardCharsets;
import java.time.Instant;
import java.time.Duration;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.springframework.boot.test.context.runner.ApplicationContextRunner;
import org.springframework.scheduling.TaskScheduler;

class NatsJetStreamMessageBusIntegrationTest {
  @Test
  void publishCreatesStreamAndDeduplicatesByOutboxMessageId() throws Exception {
    var messageId = UUID.randomUUID();
    var payload = "{\"schema_version\":1,\"message_id\":\"" + messageId + "\"}";
    var message = new OutboxMessage(
        UUID.randomUUID(), messageId, "run", UUID.randomUUID(), "run.queued", payload,
        Instant.parse("2026-07-31T08:30:00Z"), 1, UUID.randomUUID());

    try (var bus = NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings())) {
      bus.publish(message);
      bus.publish(message);
    }

    try (var observer = NativeIntegrationEnvironment.connectNats()) {
      var management = observer.jetStreamManagement();
      var stream = management.getStreamInfo(NatsJetStreamMessageBus.STREAM_NAME);
      var stored = management.getLastMessage(
          NatsJetStreamMessageBus.STREAM_NAME, OutboxSubjects.RUN_QUEUED_V1);

      assertThat(stream.getStreamState().getMsgCount()).isOne();
      assertThat(stored.getSubject()).isEqualTo(OutboxSubjects.RUN_QUEUED_V1);
      assertThat(new String(stored.getData(), StandardCharsets.UTF_8)).isEqualTo(payload);
    }
  }

  @Test
  void brokerPauseFailsClosedAndTheSameOutboxMessageIsSafeToRetryAfterResume()
      throws Exception {
    var messageId = UUID.randomUUID();
    var payload = "{\"schema_version\":1,\"message_id\":\"" + messageId + "\"}";
    var message = new OutboxMessage(
        UUID.randomUUID(), messageId, "run", UUID.randomUUID(), "run.queued", payload,
        Instant.now(), 1, UUID.randomUUID());
    var bus = NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings());
    long messagesBefore;
    try (var observer = NativeIntegrationEnvironment.connectNats()) {
      messagesBefore = observer.jetStreamManagement()
          .getStreamInfo(NatsJetStreamMessageBus.STREAM_NAME)
          .getStreamState().getMsgCount();
    }
    NativeIntegrationEnvironment.pauseNats();
    try {
      assertThatThrownBy(() -> bus.publish(message))
          .isInstanceOf(MessagePublishException.class)
          .hasMessageContaining("failed to publish outbox message");
    } finally {
      NativeIntegrationEnvironment.resumeNats();
      bus.close();
    }

    try (var recovered =
        NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings())) {
      recovered.publish(message);
    }
    try (var observer = NativeIntegrationEnvironment.connectNats()) {
      var management = observer.jetStreamManagement();
      var stream = management.getStreamInfo(NatsJetStreamMessageBus.STREAM_NAME);
      var stored = management.getLastMessage(
          NatsJetStreamMessageBus.STREAM_NAME, OutboxSubjects.RUN_QUEUED_V1);
      assertThat(stream.getStreamState().getMsgCount()).isEqualTo(messagesBefore + 1);
      assertThat(stored.getData()).isEqualTo(payload.getBytes(StandardCharsets.UTF_8));
    }
  }

  @Test
  void executionRequestIsRoutedOnlyToItsSelectedWorkerStream() throws Exception {
    var messageId = UUID.randomUUID();
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var payload = "{\"schema_version\":2,\"message_id\":\"" + messageId
        + "\",\"worker_id\":\"" + workerId + "\",\"worker_incarnation_id\":\""
        + incarnationId + "\"}";
    var message = new OutboxMessage(
        UUID.randomUUID(), messageId, "run", UUID.randomUUID(),
        "run.execution.requested", payload, Instant.now(), 1, UUID.randomUUID());

    try (var bus = NatsJetStreamMessageBus.connect(NativeIntegrationEnvironment.natsSettings())) {
      bus.publish(message);
    }

    try (var observer = NativeIntegrationEnvironment.connectNats()) {
      var management = observer.jetStreamManagement();
      var stream = management.getStreamInfo(NatsJetStreamMessageBus.EXECUTION_STREAM_NAME);
      var subject = "runtime.execution.worker." + workerId + ".incarnation."
          + incarnationId + ".run.v2";
      var stored = management.getLastMessage(
          NatsJetStreamMessageBus.EXECUTION_STREAM_NAME, subject);

      assertThat(stream.getStreamState().getMsgCount()).isOne();
      assertThat(stored.getSubject()).isEqualTo(subject);
      assertThat(new String(stored.getData(), StandardCharsets.UTF_8)).isEqualTo(payload);
    }
  }

  @Test
  void cancellationRequestUsesASeparateTargetedWorkerSubject() {
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var message = new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(),
        "run.cancellation.requested", "{\"worker_id\":\"" + workerId
            + "\",\"worker_incarnation_id\":\"" + incarnationId + "\"}",
        Instant.now(), 1, UUID.randomUUID());

    assertThat(OutboxSubjects.forMessage(message))
        .isEqualTo("runtime.execution.worker." + workerId + ".incarnation."
            + incarnationId + ".cancel.v2");
  }

  @Test
  void steeringRequestUsesItsOwnIncarnationFencedWorkerSubject() {
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var message = new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(),
        "run.steering.requested", "{\"worker_id\":\"" + workerId
            + "\",\"worker_incarnation_id\":\"" + incarnationId + "\"}",
        Instant.now(), 1, UUID.randomUUID());

    assertThat(OutboxSubjects.forMessage(message))
        .isEqualTo("runtime.execution.worker." + workerId + ".incarnation."
            + incarnationId + ".steer.v1");
  }

  @Test
  void recoveryRequestUsesTheNestedReplacementWorkerSubject() {
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var message = new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(),
        "run.recovery.requested",
        "{\"execution\":{\"worker_id\":\"" + workerId
            + "\",\"worker_incarnation_id\":\"" + incarnationId + "\"}}",
        Instant.now(), 1, UUID.randomUUID());

    assertThat(OutboxSubjects.forMessage(message))
        .isEqualTo("runtime.execution.worker." + workerId + ".incarnation."
            + incarnationId + ".restore.v2");
  }

  @Test
  void approvalDecisionUsesItsOwnTargetedWorkerSubject() {
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var message = new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(),
        "tool.approval.decided", "{\"worker_id\":\"" + workerId
            + "\",\"worker_incarnation_id\":\"" + incarnationId + "\"}",
        Instant.now(), 1, UUID.randomUUID());

    assertThat(OutboxSubjects.forMessage(message))
        .isEqualTo("runtime.execution.worker." + workerId + ".incarnation."
            + incarnationId + ".approval.v2");
  }

  @Test
  void workloadIdentityRenewalUsesItsOwnIncarnationFencedSubject() {
    var workerId = UUID.randomUUID();
    var incarnationId = UUID.randomUUID();
    var message = new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(),
        "workload.identity.renewed", "{\"worker_id\":\"" + workerId
            + "\",\"worker_incarnation_id\":\"" + incarnationId + "\"}",
        Instant.now(), 1, UUID.randomUUID());

    assertThat(OutboxSubjects.forMessage(message))
        .isEqualTo("runtime.execution.worker." + workerId + ".incarnation."
            + incarnationId + ".identity.v1");
  }

  @Test
  void unknownEventTypeCannotEscapeIntoAnAdHocSubject() {
    var message = new OutboxMessage(
        UUID.randomUUID(), UUID.randomUUID(), "run", UUID.randomUUID(), "run.typo", "{}",
        Instant.now(), 1, UUID.randomUUID());

    assertThatThrownBy(() -> OutboxSubjects.forMessage(message))
        .isInstanceOf(IllegalArgumentException.class)
        .hasMessage("unsupported outbox event type run.typo");
  }

  @Test
  void enabledOutboxConfigurationCreatesAWorkingScheduledPublisher() {
    OutboxStore emptyStore = new OutboxStore() {
      @Override
      public List<OutboxMessage> claimNext(
          int limit, UUID claimToken, Duration leaseDuration) {
        return List.of();
      }

      @Override
      public boolean markPublished(UUID tenantId, UUID messageId, UUID claimToken) {
        return false;
      }

      @Override
      public boolean release(
          UUID tenantId, UUID messageId, UUID claimToken, String failureMessage) {
        return false;
      }
    };

    new ApplicationContextRunner()
        .withUserConfiguration(OutboxPublisherConfiguration.class)
        .withBean(OutboxStore.class, () -> emptyStore)
        .withPropertyValues(
            NativeIntegrationEnvironment.natsSecurityProperties("agent.runtime.outbox"))
        .withPropertyValues(
            "agent.runtime.outbox.enabled=true",
            "agent.runtime.outbox.batch-size=25",
            "agent.runtime.outbox.claim-duration=20s",
            "agent.runtime.outbox.poll-delay-ms=250")
        .run(context -> {
          assertThat(context).hasSingleBean(MessageBus.class);
          assertThat(context).hasSingleBean(OutboxPublisher.class);
          assertThat(context).hasSingleBean(ScheduledOutboxPublisher.class);
          assertThat(context).hasSingleBean(TaskScheduler.class);
          assertThat(context.getBean(ScheduledOutboxPublisher.class).poll())
              .isEqualTo(new OutboxPublishResult(0, 0, 0));
        });
  }
}
