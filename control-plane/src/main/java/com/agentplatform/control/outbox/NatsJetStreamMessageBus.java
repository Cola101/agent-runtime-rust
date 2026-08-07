package com.agentplatform.control.outbox;

import com.agentplatform.control.messaging.NatsConnectionSettings;
import io.nats.client.Connection;
import io.nats.client.JetStream;
import io.nats.client.Nats;
import io.nats.client.PublishOptions;
import java.nio.charset.StandardCharsets;
import java.util.Objects;

public final class NatsJetStreamMessageBus implements MessageBus, AutoCloseable {
  public static final String STREAM_NAME = RuntimeNatsTopology.CONTROL_STREAM;
  public static final String EXECUTION_STREAM_NAME = RuntimeNatsTopology.EXECUTION_STREAM;

  private final Connection connection;
  private final JetStream jetStream;

  private NatsJetStreamMessageBus(Connection connection) {
    this.connection = Objects.requireNonNull(connection);
    try {
      RuntimeNatsTopology.ensure(connection);
      this.jetStream = connection.jetStream();
    } catch (Exception exception) {
      closeAfterInitializationFailure(connection, exception);
      throw new MessagePublishException("failed to initialize JetStream outbox stream", exception);
    }
  }

  public static NatsJetStreamMessageBus connect(String natsUrl) {
    return connect(NatsConnectionSettings.insecureForDevelopment(natsUrl));
  }

  public static NatsJetStreamMessageBus connect(NatsConnectionSettings settings) {
    try {
      return new NatsJetStreamMessageBus(Nats.connect(Objects.requireNonNull(settings).toOptions()));
    } catch (MessagePublishException exception) {
      throw exception;
    } catch (Exception exception) {
      throw new MessagePublishException("failed to connect to NATS", exception);
    }
  }

  @Override
  public void publish(OutboxMessage message) {
    Objects.requireNonNull(message, "message");
    var subject = OutboxSubjects.forMessage(message);
    var options = PublishOptions.builder()
        .expectedStream(streamFor(subject))
        .messageId(message.id().toString())
        .build();
    try {
      jetStream.publish(subject, message.payload().getBytes(StandardCharsets.UTF_8), options);
    } catch (Exception exception) {
      throw new MessagePublishException("failed to publish outbox message " + message.id(), exception);
    }
  }

  private static String streamFor(String subject) {
    if (subject.startsWith("runtime.control.")) {
      return STREAM_NAME;
    }
    if (subject.startsWith("runtime.execution.")) {
      return EXECUTION_STREAM_NAME;
    }
    throw new IllegalArgumentException("unsupported runtime subject " + subject);
  }

  @Override
  public void close() throws InterruptedException {
    connection.close();
  }

  private static void closeAfterInitializationFailure(Connection connection, Exception failure) {
    try {
      connection.close();
    } catch (InterruptedException interrupted) {
      Thread.currentThread().interrupt();
      failure.addSuppressed(interrupted);
    }
  }
}
