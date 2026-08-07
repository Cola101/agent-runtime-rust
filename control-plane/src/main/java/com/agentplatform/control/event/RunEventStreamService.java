package com.agentplatform.control.event;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.time.Duration;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.springframework.http.MediaType;
import org.springframework.stereotype.Service;
import org.springframework.web.servlet.mvc.method.annotation.SseEmitter;

@Service
public class RunEventStreamService {
  private static final Duration STREAM_TIMEOUT = Duration.ofMinutes(30);
  private static final Set<String> TERMINAL_EVENTS = Set.of(
      "run.succeeded", "run.failed", "run.cancelled", "run.timed_out", "run.indeterminate");

  private final JdbcRunEventRepository events;
  private final ScheduledExecutorService poller;
  private final ObjectMapper objectMapper;

  public RunEventStreamService(
      JdbcRunEventRepository events,
      ScheduledExecutorService poller,
      ObjectMapper objectMapper) {
    this.events = events;
    this.poller = poller;
    this.objectMapper = objectMapper;
  }

  public SseEmitter open(
      UUID tenantId, UUID applicationId, UUID runId, UUID lastEventId) {
    var emitter = new SseEmitter(STREAM_TIMEOUT.toMillis());
    var cursor = new AtomicReference<>(lastEventId);
    var task = new AtomicReference<ScheduledFuture<?>>();
    Runnable cancel = () -> {
      var scheduled = task.get();
      if (scheduled != null) scheduled.cancel(false);
    };
    emitter.onCompletion(cancel);
    emitter.onTimeout(() -> {
      cancel.run();
      emitter.complete();
    });
    emitter.onError(error -> cancel.run());

    boolean terminal = replay(tenantId, applicationId, runId, cursor, emitter);
    if (terminal) {
      emitter.complete();
      return emitter;
    }
    task.set(poller.scheduleWithFixedDelay(() -> {
      try {
        if (replay(tenantId, applicationId, runId, cursor, emitter)) emitter.complete();
      } catch (RuntimeException error) {
        emitter.completeWithError(error);
      }
    }, 500, 500, TimeUnit.MILLISECONDS));
    return emitter;
  }

  private boolean replay(
      UUID tenantId,
      UUID applicationId,
      UUID runId,
      AtomicReference<UUID> cursor,
      SseEmitter emitter) {
    for (var event : events.findAfter(
        tenantId, applicationId, runId, cursor.get(), 100)) {
      try {
        emitter.send(SseEmitter.event()
            .id(event.eventId().toString())
            .name(event.type())
            .data(objectMapper.readTree(event.payload()), MediaType.APPLICATION_JSON));
      } catch (IOException error) {
        throw new IllegalStateException("failed to write run event stream", error);
      }
      cursor.set(event.eventId());
      if (TERMINAL_EVENTS.contains(event.type())) return true;
    }
    return false;
  }
}
