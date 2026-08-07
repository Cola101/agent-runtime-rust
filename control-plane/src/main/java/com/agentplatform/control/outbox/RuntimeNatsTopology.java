package com.agentplatform.control.outbox;

import io.nats.client.Connection;
import io.nats.client.api.StorageType;
import io.nats.client.api.StreamConfiguration;
import java.time.Duration;

public final class RuntimeNatsTopology {
  public static final String CONTROL_STREAM = "RUNTIME_CONTROL";
  public static final String EXECUTION_STREAM = "RUNTIME_EXECUTION";
  public static final String WORKER_EVENT_STREAM = "RUNTIME_WORKER";
  public static final String WORKER_EVENT_WILDCARD = "runtime.worker.>";
  private static final Duration DUPLICATE_WINDOW = Duration.ofHours(24);

  private RuntimeNatsTopology() {}

  public static void ensure(Connection connection) throws Exception {
    var management = connection.jetStreamManagement();
    var names = management.getStreamNames();
    ensureStream(names, management, CONTROL_STREAM, OutboxSubjects.RUNTIME_CONTROL_WILDCARD);
    ensureStream(names, management, EXECUTION_STREAM, OutboxSubjects.RUNTIME_EXECUTION_WILDCARD);
    ensureStream(names, management, WORKER_EVENT_STREAM, WORKER_EVENT_WILDCARD);
  }

  private static void ensureStream(
      java.util.List<String> streamNames,
      io.nats.client.JetStreamManagement management,
      String name,
      String subject) throws Exception {
    if (streamNames.contains(name)) {
      return;
    }
    management.addStream(StreamConfiguration.builder()
        .name(name)
        .subjects(subject)
        .storageType(StorageType.File)
        .duplicateWindow(DUPLICATE_WINDOW)
        .build());
  }
}
