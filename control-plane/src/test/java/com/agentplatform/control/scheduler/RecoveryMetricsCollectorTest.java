package com.agentplatform.control.scheduler;

import static org.assertj.core.api.Assertions.assertThat;

import io.micrometer.core.instrument.simple.SimpleMeterRegistry;
import io.micrometer.prometheusmetrics.PrometheusConfig;
import io.micrometer.prometheusmetrics.PrometheusMeterRegistry;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.time.ZoneId;
import java.time.ZoneOffset;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;

class RecoveryMetricsCollectorTest {
  @Test
  void rendersPrometheusNamesAndNeverSerializesTenantIdentity() {
    RecoveryMetricsSource source = objective ->
        new RecoverySloSnapshot(3, 1, 2, 1, 960_000);
    var registry = new PrometheusMeterRegistry(PrometheusConfig.DEFAULT);
    var collector = new RecoveryMetricsCollector(
        source, Duration.ofMinutes(15), Clock.systemUTC());
    collector.bindTo(registry);

    collector.refresh();

    assertThat(registry.scrape())
        .contains("agent_runtime_recovery_incidents{state=\"open\"} 3.0")
        .contains("agent_runtime_recovery_incidents{state=\"overdue\"} 1.0")
        .contains("agent_runtime_recovery_oldest_open_seconds 960.0")
        .doesNotContain("tenant_id");
  }

  @Test
  void publishesLowCardinalityRecoveryGaugesWithoutTenantTags() {
    var snapshot = new AtomicReference<>(new RecoverySloSnapshot(3, 1, 2, 1, 960_000));
    RecoveryMetricsSource source = objective -> snapshot.get();
    var clock = new MutableClock(Instant.parse("2026-08-01T12:00:00Z"));
    var registry = new SimpleMeterRegistry();
    var collector = new RecoveryMetricsCollector(
        source, Duration.ofMinutes(15), clock);
    collector.bindTo(registry);

    collector.refresh();

    assertThat(gauge(registry, "agent.runtime.recovery.incidents", "open")).isEqualTo(3);
    assertThat(gauge(registry, "agent.runtime.recovery.incidents", "overdue")).isEqualTo(1);
    assertThat(gauge(registry, "agent.runtime.recovery.incidents", "waiting_capacity"))
        .isEqualTo(2);
    assertThat(gauge(registry, "agent.runtime.recovery.incidents", "recovery_requested"))
        .isEqualTo(1);
    assertThat(registry.get("agent.runtime.recovery.oldest.open.seconds").gauge().value())
        .isEqualTo(960);
    assertThat(registry.getMeters())
        .allSatisfy(meter -> assertThat(meter.getId().getTags())
            .noneMatch(tag -> tag.getKey().equals("tenant_id")));
  }

  @Test
  void failedRefreshKeepsTheLastSnapshotAndMakesCollectionStalenessObservable() {
    var fail = new AtomicBoolean();
    RecoveryMetricsSource source = objective -> {
      if (fail.get()) {
        throw new IllegalStateException("injected database outage");
      }
      return new RecoverySloSnapshot(2, 0, 1, 1, 60_000);
    };
    var clock = new MutableClock(Instant.parse("2026-08-01T12:00:00Z"));
    var registry = new SimpleMeterRegistry();
    var collector = new RecoveryMetricsCollector(
        source, Duration.ofMinutes(15), clock);
    collector.bindTo(registry);
    collector.refresh();
    var lastSuccess = registry.get("agent.runtime.recovery.metrics.last.success.seconds")
        .gauge().value();

    fail.set(true);
    clock.advance(Duration.ofMinutes(1));
    collector.refresh();

    assertThat(gauge(registry, "agent.runtime.recovery.incidents", "open")).isEqualTo(2);
    assertThat(registry.get("agent.runtime.recovery.metrics.last.success.seconds")
        .gauge().value()).isEqualTo(lastSuccess);
    assertThat(registry.get("agent.runtime.recovery.metrics.refresh.errors")
        .functionCounter().count())
        .isEqualTo(1);
  }

  private double gauge(SimpleMeterRegistry registry, String name, String state) {
    return registry.get(name).tag("state", state).gauge().value();
  }

  private static final class MutableClock extends Clock {
    private Instant instant;

    private MutableClock(Instant instant) {
      this.instant = instant;
    }

    private void advance(Duration duration) {
      instant = instant.plus(duration);
    }

    @Override
    public ZoneId getZone() {
      return ZoneOffset.UTC;
    }

    @Override
    public Clock withZone(ZoneId zone) {
      return this;
    }

    @Override
    public Instant instant() {
      return instant;
    }
  }
}
