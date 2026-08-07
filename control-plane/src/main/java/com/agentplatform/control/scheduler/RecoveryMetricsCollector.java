package com.agentplatform.control.scheduler;

import io.micrometer.core.instrument.Gauge;
import io.micrometer.core.instrument.MeterRegistry;
import io.micrometer.core.instrument.binder.MeterBinder;
import java.time.Clock;
import java.time.Duration;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.scheduling.annotation.Scheduled;

public final class RecoveryMetricsCollector implements MeterBinder {
  private static final Logger LOGGER = LoggerFactory.getLogger(RecoveryMetricsCollector.class);
  private static final RecoverySloSnapshot EMPTY = new RecoverySloSnapshot(0, 0, 0, 0, 0);

  private final RecoveryMetricsSource source;
  private final Duration objective;
  private final Clock clock;
  private final AtomicReference<RecoverySloSnapshot> snapshot = new AtomicReference<>(EMPTY);
  private final AtomicLong lastSuccessEpochMillis = new AtomicLong();
  private final AtomicLong refreshErrors = new AtomicLong();

  public RecoveryMetricsCollector(
      RecoveryMetricsSource source, Duration objective, Clock clock) {
    this.source = Objects.requireNonNull(source);
    this.objective = Objects.requireNonNull(objective);
    this.clock = Objects.requireNonNull(clock);
    if (objective.isZero() || objective.isNegative()
        || objective.compareTo(Duration.ofHours(24)) > 0) {
      throw new IllegalArgumentException("recovery objective must be between 1ms and 24 hours");
    }
  }

  @Scheduled(fixedDelayString =
      "${agent.runtime.scheduler.recovery-metrics-poll-delay-ms:5000}")
  public void refresh() {
    try {
      snapshot.set(source.globalRecoverySloSnapshot(objective));
      lastSuccessEpochMillis.set(clock.millis());
    } catch (RuntimeException exception) {
      refreshErrors.incrementAndGet();
      LOGGER.warn("Failed to refresh recovery metrics; retaining the last successful snapshot",
          exception);
    }
  }

  @Override
  public void bindTo(MeterRegistry registry) {
    registerIncidentGauge(registry, "open", RecoverySloSnapshot::openIncidents);
    registerIncidentGauge(registry, "overdue", RecoverySloSnapshot::overdueIncidents);
    registerIncidentGauge(registry, "waiting_capacity", RecoverySloSnapshot::waitingCapacity);
    registerIncidentGauge(registry, "recovery_requested",
        RecoverySloSnapshot::recoveryRequested);
    Gauge.builder("agent.runtime.recovery.oldest.open.seconds", snapshot,
            value -> value.get().oldestOpenAgeMillis() / 1000.0)
        .description("Age in seconds of the oldest unresolved recovery incident")
        .register(registry);
    Gauge.builder("agent.runtime.recovery.metrics.last.success.seconds",
            lastSuccessEpochMillis, value -> value.get() / 1000.0)
        .description("Unix timestamp of the last successful recovery metric refresh")
        .register(registry);
    io.micrometer.core.instrument.FunctionCounter.builder(
            "agent.runtime.recovery.metrics.refresh.errors", refreshErrors, AtomicLong::get)
        .description("Number of failed recovery metric refreshes")
        .register(registry);
  }

  private void registerIncidentGauge(
      MeterRegistry registry,
      String state,
      java.util.function.ToIntFunction<RecoverySloSnapshot> value) {
    Gauge.builder("agent.runtime.recovery.incidents", snapshot,
            current -> value.applyAsInt(current.get()))
        .description("Current recovery incidents by low-cardinality operational state")
        .tag("state", state)
        .register(registry);
  }
}
