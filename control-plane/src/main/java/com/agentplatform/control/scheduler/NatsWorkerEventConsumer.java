package com.agentplatform.control.scheduler;

import com.agentplatform.control.messaging.NatsConnectionSettings;
import com.agentplatform.control.outbox.MessagePublishException;
import com.agentplatform.control.outbox.RuntimeNatsTopology;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.nats.client.Connection;
import io.nats.client.JetStreamSubscription;
import io.nats.client.Nats;
import io.nats.client.api.AckPolicy;
import io.nats.client.api.ConsumerConfiguration;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Base64;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicBoolean;

public final class NatsWorkerEventConsumer implements AutoCloseable {
  public static final String HEARTBEAT_SUBJECT = "runtime.worker.heartbeat.v2";
  public static final String ACCEPTED_SUBJECT = "runtime.worker.execution.accepted.v2";
  public static final String RUN_EVENT_SUBJECT = "runtime.worker.run.event.v1";
  public static final String CHECKPOINT_SUBJECT = "runtime.worker.run.checkpoint.v1";
  public static final String STEERING_OUTCOME_SUBJECT =
      "runtime.worker.run.steering.outcome.v1";
  private static final ObjectMapper JSON = new ObjectMapper();

  private final Connection connection;
  private final JetStreamSubscription heartbeats;
  private final JetStreamSubscription acceptances;
  private final JetStreamSubscription runEvents;
  private final JetStreamSubscription checkpoints;
  private final JetStreamSubscription steeringOutcomes;
  private final WorkerEventHandler handler;
  private final AtomicBoolean closed = new AtomicBoolean();

  private NatsWorkerEventConsumer(
      Connection connection,
      JetStreamSubscription heartbeats,
      JetStreamSubscription acceptances,
      JetStreamSubscription runEvents,
      JetStreamSubscription checkpoints,
      JetStreamSubscription steeringOutcomes,
      WorkerEventHandler handler) {
    this.connection = connection;
    this.heartbeats = heartbeats;
    this.acceptances = acceptances;
    this.runEvents = runEvents;
    this.checkpoints = checkpoints;
    this.steeringOutcomes = steeringOutcomes;
    this.handler = handler;
  }

  public static NatsWorkerEventConsumer connect(
      String natsUrl, String durablePrefix, WorkerEventHandler handler) {
    return connect(
        NatsConnectionSettings.insecureForDevelopment(natsUrl), durablePrefix, handler);
  }

  public static NatsWorkerEventConsumer connect(
      NatsConnectionSettings settings, String durablePrefix, WorkerEventHandler handler) {
    if (durablePrefix == null || durablePrefix.isBlank()) {
      throw new IllegalArgumentException("worker event durable prefix must not be blank");
    }
    Connection connection = null;
    try {
      connection = Nats.connect(settings.toOptions());
      RuntimeNatsTopology.ensure(connection);
      var jetstream = connection.jetStream();
      var heartbeats = jetstream.subscribe(
          HEARTBEAT_SUBJECT, pullOptions(durablePrefix + "-heartbeats", HEARTBEAT_SUBJECT));
      var acceptances = jetstream.subscribe(
          ACCEPTED_SUBJECT, pullOptions(durablePrefix + "-acceptances", ACCEPTED_SUBJECT));
      var runEvents = jetstream.subscribe(
          RUN_EVENT_SUBJECT, pullOptions(durablePrefix + "-run-events", RUN_EVENT_SUBJECT));
      var checkpoints = jetstream.subscribe(
          CHECKPOINT_SUBJECT, pullOptions(durablePrefix + "-checkpoints", CHECKPOINT_SUBJECT));
      var steeringOutcomes = jetstream.subscribe(
          STEERING_OUTCOME_SUBJECT,
          pullOptions(durablePrefix + "-steering-outcomes", STEERING_OUTCOME_SUBJECT));
      return new NatsWorkerEventConsumer(
          connection, heartbeats, acceptances, runEvents, checkpoints, steeringOutcomes, handler);
    } catch (Exception exception) {
      closeAfterFailure(connection, exception);
      throw new MessagePublishException("failed to initialize worker event consumer", exception);
    }
  }

  public WorkerEventPollResult poll(Duration timeout) {
    if (timeout == null || timeout.isZero() || timeout.isNegative()) {
      throw new IllegalArgumentException("worker event poll timeout must be positive");
    }
    if (closed.get()) {
      return WorkerEventPollResult.IDLE;
    }
    var sliceTimeout = timeout.dividedBy(5);
    if (sliceTimeout.isZero()) {
      sliceTimeout = timeout;
    }
    try {
      var heartbeatMessages = heartbeats.fetch(1, sliceTimeout);
      if (!heartbeatMessages.isEmpty()) {
        var message = heartbeatMessages.getFirst();
        try {
          handler.onHeartbeat(decodeHeartbeat(message.getData()));
          message.ackSync(Duration.ofSeconds(2));
          return WorkerEventPollResult.HEARTBEAT_RECORDED;
        } catch (IllegalArgumentException malformed) {
          message.term();
          return WorkerEventPollResult.TERMINATED;
        } catch (RuntimeException retryable) {
          message.nakWithDelay(Duration.ofSeconds(1));
          throw retryable;
        }
      }

      var acceptedMessages = acceptances.fetch(1, sliceTimeout);
      if (!acceptedMessages.isEmpty()) {
        var message = acceptedMessages.getFirst();
        try {
          var recorded = handler.onAccepted(decodeAcceptance(message.getData()));
          if (recorded) {
            message.ackSync(Duration.ofSeconds(2));
            return WorkerEventPollResult.ACCEPTANCE_RECORDED;
          }
          message.nakWithDelay(Duration.ofSeconds(1));
          return WorkerEventPollResult.RETRY_SCHEDULED;
        } catch (IllegalArgumentException malformed) {
          message.term();
          return WorkerEventPollResult.TERMINATED;
        } catch (RuntimeException retryable) {
          message.nakWithDelay(Duration.ofSeconds(1));
          throw retryable;
        }
      }

      var eventMessages = runEvents.fetch(1, sliceTimeout);
      if (!eventMessages.isEmpty()) {
        var message = eventMessages.getFirst();
        try {
          var recorded = handler.onRunEvent(decodeRunEvent(message.getData()));
          if (recorded) {
            message.ackSync(Duration.ofSeconds(2));
            return WorkerEventPollResult.RUN_EVENT_RECORDED;
          }
          message.nakWithDelay(Duration.ofSeconds(1));
          return WorkerEventPollResult.RETRY_SCHEDULED;
        } catch (IllegalArgumentException malformed) {
          message.term();
          return WorkerEventPollResult.TERMINATED;
        } catch (RuntimeException retryable) {
          message.nakWithDelay(Duration.ofSeconds(1));
          throw retryable;
        }
      }

      var checkpointMessages = checkpoints.fetch(1, sliceTimeout);
      if (!checkpointMessages.isEmpty()) {
        var message = checkpointMessages.getFirst();
        try {
          var recorded = handler.onCheckpoint(decodeCheckpoint(message.getData()));
          if (recorded) {
            message.ackSync(Duration.ofSeconds(2));
            return WorkerEventPollResult.CHECKPOINT_RECORDED;
          }
          message.nakWithDelay(Duration.ofSeconds(1));
          return WorkerEventPollResult.RETRY_SCHEDULED;
        } catch (IllegalArgumentException malformed) {
          message.term();
          return WorkerEventPollResult.TERMINATED;
        } catch (RuntimeException retryable) {
          message.nakWithDelay(Duration.ofSeconds(1));
          throw retryable;
        }
      }

      var outcomeMessages = steeringOutcomes.fetch(1, sliceTimeout);
      if (outcomeMessages.isEmpty()) {
        return WorkerEventPollResult.IDLE;
      }
      var message = outcomeMessages.getFirst();
      try {
        handler.onSteeringOutcome(decodeSteeringOutcome(message.getData()));
        message.ackSync(Duration.ofSeconds(2));
        return WorkerEventPollResult.STEERING_OUTCOME_RECORDED;
      } catch (IllegalArgumentException malformed) {
        message.term();
        return WorkerEventPollResult.TERMINATED;
      } catch (RuntimeException retryable) {
        message.nakWithDelay(Duration.ofSeconds(1));
        throw retryable;
      }
    } catch (InterruptedException interrupted) {
      Thread.currentThread().interrupt();
      throw new MessagePublishException("worker event polling was interrupted", interrupted);
    } catch (RuntimeException exception) {
      if (closed.get()) {
        return WorkerEventPollResult.IDLE;
      }
      throw exception;
    } catch (Exception exception) {
      if (closed.get()) {
        return WorkerEventPollResult.IDLE;
      }
      throw new MessagePublishException("failed to poll worker events", exception);
    }
  }

  private WorkerHeartbeatMessage decodeHeartbeat(byte[] data) {
    try {
      var root = JSON.readTree(data);
      var placements = new ArrayList<String>();
      root.path("placements").forEach(value -> placements.add(value.asText()));
      var assignments = new ArrayList<ActiveAssignmentMessage>();
      root.path("active_assignments").forEach(value -> assignments.add(
          new ActiveAssignmentMessage(
              uuid(value, "tenant_id"),
              uuid(value, "run_id"),
              uuid(value, "attempt_id"),
              uuid(value, "workspace_id"),
              value.path("owner_epoch").asLong(),
              uuid(value, "fencing_token"))));
      return new WorkerHeartbeatMessage(
          root.path("schema_version").asInt(),
          uuid(root, "message_id"),
          uuid(root, "worker_id"),
          uuid(root, "incarnation_id"),
          Instant.parse(root.path("occurred_at").asText()),
          placements,
          root.path("capacity").asInt(),
          root.path("active_runs").asInt(),
          assignments,
          root.path("runtime_version").asText(),
          root.path("accepting_work").asBoolean(true),
          root.hasNonNull("draining_since")
              ? Instant.parse(root.path("draining_since").asText()) : null,
          root.hasNonNull("drain_deadline")
              ? Instant.parse(root.path("drain_deadline").asText()) : null);
    } catch (Exception exception) {
      throw new IllegalArgumentException("invalid worker heartbeat", exception);
    }
  }

  private ExecutionAcceptedMessage decodeAcceptance(byte[] data) {
    try {
      var root = JSON.readTree(data);
      return new ExecutionAcceptedMessage(
          root.path("schema_version").asInt(),
          uuid(root, "message_id"),
          uuid(root, "tenant_id"),
          uuid(root, "run_id"),
          uuid(root, "attempt_id"),
          uuid(root, "worker_id"),
          uuid(root, "worker_incarnation_id"),
          Instant.parse(root.path("accepted_at").asText()));
    } catch (Exception exception) {
      throw new IllegalArgumentException("invalid execution acceptance", exception);
    }
  }

  private RunEventMessage decodeRunEvent(byte[] data) {
    try {
      var root = JSON.readTree(data);
      return new RunEventMessage(
          uuid(root, "event_id"),
          root.path("schema_version").asInt(),
          uuid(root, "tenant_id"),
          uuid(root, "session_id"),
          uuid(root, "run_id"),
          root.path("sequence").asLong(),
          uuid(root, "attempt_id"),
          Instant.parse(root.path("timestamp").asText()),
          uuid(root, "trace_id"),
          root.path("type").asText(),
          JSON.writeValueAsString(root.path("payload")),
          root.path("digest").asText());
    } catch (Exception exception) {
      throw new IllegalArgumentException("invalid worker run event", exception);
    }
  }

  private RunCheckpointMessage decodeCheckpoint(byte[] data) {
    try {
      var root = JSON.readTree(data);
      var schemaVersion = root.path("schema_version").asInt();
      var payload = root.hasNonNull("payload_base64")
          ? Base64.getDecoder().decode(root.path("payload_base64").asText())
          : null;
      var payloadDigest = root.path("payload_digest").asText();
      return new RunCheckpointMessage(
          schemaVersion,
          uuid(root, "message_id"),
          uuid(root, "tenant_id"),
          uuid(root, "run_id"),
          uuid(root, "session_id"),
          uuid(root, "attempt_id"),
          root.path("owner_epoch").asLong(),
          uuid(root, "fencing_token"),
          root.path("sequence").asLong(),
          root.path("status").asText(),
          root.path("kernel_digest").asText(),
          root.path("tool_catalog_digest").asText(),
          payload,
          root.hasNonNull("payload_ref") ? root.path("payload_ref").asText() : null,
          root.path("payload_encoding").asText(schemaVersion == 1 ? "identity" : ""),
          payloadDigest,
          root.path("stored_payload_digest").asText(payloadDigest),
          root.path("uncompressed_size").asLong(payload == null ? 0 : payload.length),
          root.path("stored_size").asLong(payload == null ? 0 : payload.length),
          Instant.parse(root.path("created_at").asText()));
    } catch (Exception exception) {
      throw new IllegalArgumentException("invalid worker checkpoint", exception);
    }
  }

  private RunSteeringOutcomeMessage decodeSteeringOutcome(byte[] data) {
    try {
      var root = JSON.readTree(data);
      return new RunSteeringOutcomeMessage(
          root.path("schema_version").asInt(),
          uuid(root, "message_id"),
          uuid(root, "steering_id"),
          uuid(root, "tenant_id"),
          uuid(root, "run_id"),
          uuid(root, "attempt_id"),
          uuid(root, "worker_id"),
          uuid(root, "worker_incarnation_id"),
          root.path("input_digest").asText(),
          root.path("outcome").asText(),
          root.path("reason").asText(),
          Instant.parse(root.path("occurred_at").asText()));
    } catch (Exception exception) {
      throw new IllegalArgumentException("invalid run steering outcome", exception);
    }
  }

  private UUID uuid(JsonNode root, String field) {
    return UUID.fromString(root.path(field).asText());
  }

  private static io.nats.client.PullSubscribeOptions pullOptions(
      String durableName, String subject) {
    return ConsumerConfiguration.builder()
        .durable(durableName)
        .ackPolicy(AckPolicy.Explicit)
        .ackWait(Duration.ofSeconds(30))
        .maxDeliver(20)
        .filterSubject(subject)
        .buildPullSubscribeOptions(RuntimeNatsTopology.WORKER_EVENT_STREAM);
  }

  @Override
  public void close() throws InterruptedException {
    closed.set(true);
    heartbeats.unsubscribe();
    acceptances.unsubscribe();
    runEvents.unsubscribe();
    checkpoints.unsubscribe();
    steeringOutcomes.unsubscribe();
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
