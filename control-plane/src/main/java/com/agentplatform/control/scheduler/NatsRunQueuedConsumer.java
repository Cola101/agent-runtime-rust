package com.agentplatform.control.scheduler;

import com.agentplatform.control.messaging.NatsConnectionSettings;
import com.agentplatform.control.outbox.MessagePublishException;
import com.agentplatform.control.outbox.NatsJetStreamMessageBus;
import com.agentplatform.control.outbox.OutboxSubjects;
import com.agentplatform.control.outbox.RuntimeNatsTopology;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.nats.client.Connection;
import io.nats.client.JetStreamSubscription;
import io.nats.client.Nats;
import io.nats.client.api.AckPolicy;
import io.nats.client.api.ConsumerConfiguration;
import java.time.Duration;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicBoolean;

public final class NatsRunQueuedConsumer implements AutoCloseable {
  private static final ObjectMapper JSON = new ObjectMapper();

  private final Connection connection;
  private final JetStreamSubscription subscription;
  private final RunQueuedHandler handler;
  private final Duration retryDelay;
  private final AtomicBoolean closed = new AtomicBoolean();

  private NatsRunQueuedConsumer(
      Connection connection,
      JetStreamSubscription subscription,
      RunQueuedHandler handler,
      Duration retryDelay) {
    this.connection = connection;
    this.subscription = subscription;
    this.handler = handler;
    this.retryDelay = retryDelay;
  }

  public static NatsRunQueuedConsumer connect(
      String natsUrl, String durableName, RunQueuedHandler handler, Duration retryDelay) {
    return connect(
        NatsConnectionSettings.insecureForDevelopment(natsUrl),
        durableName,
        handler,
        retryDelay);
  }

  public static NatsRunQueuedConsumer connect(
      NatsConnectionSettings settings,
      String durableName,
      RunQueuedHandler handler,
      Duration retryDelay) {
    Objects.requireNonNull(handler, "handler");
    if (durableName == null || durableName.isBlank()) {
      throw new IllegalArgumentException("scheduler durable name must not be blank");
    }
    if (retryDelay == null || retryDelay.isNegative() || retryDelay.isZero()) {
      throw new IllegalArgumentException("scheduler retry delay must be positive");
    }
    Connection connection = null;
    try {
      connection = Nats.connect(Objects.requireNonNull(settings).toOptions());
      RuntimeNatsTopology.ensure(connection);
      var configuration = ConsumerConfiguration.builder()
          .durable(durableName)
          .ackPolicy(AckPolicy.Explicit)
          .ackWait(Duration.ofSeconds(30))
          .maxDeliver(20)
          .filterSubject(OutboxSubjects.RUN_QUEUED_V1)
          .buildPullSubscribeOptions(NatsJetStreamMessageBus.STREAM_NAME);
      var subscription = connection.jetStream().subscribe(
          OutboxSubjects.RUN_QUEUED_V1, configuration);
      return new NatsRunQueuedConsumer(connection, subscription, handler, retryDelay);
    } catch (Exception exception) {
      closeAfterFailure(connection, exception);
      throw new MessagePublishException("failed to initialize RunQueued consumer", exception);
    }
  }

  public SchedulerPollResult poll(Duration timeout) {
    if (timeout == null || timeout.isNegative() || timeout.isZero()) {
      throw new IllegalArgumentException("scheduler poll timeout must be positive");
    }
    if (closed.get()) {
      return SchedulerPollResult.IDLE;
    }
    try {
      var messages = subscription.fetch(1, timeout);
      if (messages.isEmpty()) {
        return SchedulerPollResult.IDLE;
      }
      var message = messages.getFirst();
      QueuedRun queuedRun;
      try {
        queuedRun = decode(message.getData());
      } catch (RuntimeException malformed) {
        message.term();
        return SchedulerPollResult.TERMINATED;
      }

      ScheduleResult result;
      try {
        result = handler.handle(queuedRun);
      } catch (RuntimeException failure) {
        message.nakWithDelay(retryDelay);
        return SchedulerPollResult.RETRY_SCHEDULED;
      }
      if (result.status() == ScheduleStatus.RETRY_NO_CAPACITY
          || result.status() == ScheduleStatus.RETRY_WORKSPACE_BUSY) {
        message.nakWithDelay(retryDelay);
        return SchedulerPollResult.RETRY_SCHEDULED;
      }
      message.ackSync(Duration.ofSeconds(2));
      return SchedulerPollResult.ACKED;
    } catch (InterruptedException interrupted) {
      Thread.currentThread().interrupt();
      throw new MessagePublishException("RunQueued polling was interrupted", interrupted);
    } catch (Exception exception) {
      if (closed.get()) {
        return SchedulerPollResult.IDLE;
      }
      throw new MessagePublishException("failed to poll RunQueued", exception);
    }
  }

  private QueuedRun decode(byte[] data) {
    try {
      var root = JSON.readTree(data);
      if (root.path("schema_version").asInt() != 1) {
        throw new IllegalArgumentException("unsupported RunQueued schema version");
      }
      return new QueuedRun(
          UUID.fromString(root.path("tenant_id").asText()),
          UUID.fromString(root.path("run_id").asText()));
    } catch (Exception exception) {
      throw new IllegalArgumentException("invalid RunQueued message", exception);
    }
  }

  @Override
  public void close() throws InterruptedException {
    closed.set(true);
    subscription.unsubscribe();
    connection.close();
  }

  private static void closeAfterFailure(Connection connection, Exception failure) {
    if (connection == null) {
      return;
    }
    try {
      connection.close();
    } catch (InterruptedException interrupted) {
      Thread.currentThread().interrupt();
      failure.addSuppressed(interrupted);
    }
  }
}
