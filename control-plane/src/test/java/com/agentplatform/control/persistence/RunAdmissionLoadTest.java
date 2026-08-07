package com.agentplatform.control.persistence;

import static org.assertj.core.api.Assertions.assertThat;

import com.agentplatform.control.outbox.JdbcOutboxRepository;
import com.agentplatform.control.outbox.OutboxMessage;
import com.agentplatform.control.run.Run;
import com.agentplatform.control.run.RunStatus;
import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.atomic.AtomicInteger;
import javax.sql.DataSource;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.jdbc.datasource.DriverManagerDataSource;
import org.springframework.transaction.support.TransactionTemplate;

/**
 * Admission and dispatch at 1000 Runs.
 *
 * <p>Everything before this was measured on a handful of rows, where a quota
 * that double-counts and a drain that is quadratic both look fine. The numbers
 * this prints are the point as much as the assertions are.
 *
 * <p>Scope is the control plane: admission, the outbox, and the drain. It does
 * not execute the Runs, so it says nothing about Worker throughput or model
 * latency, and nothing here should be read as an end-to-end figure.
 */
class RunAdmissionLoadTest {
  private static final int TENANTS = 10;
  private static final int RUNS_PER_TENANT = 100;
  private static final int TOTAL_RUNS = TENANTS * RUNS_PER_TENANT;
  private static final int WRITERS = 8;
  private static final int DRAIN_BATCH = 100;

  /**
   * Generous on purpose. It is not a performance target -- it is a trip wire for
   * an accidentally quadratic drain, which at this backlog would blow past it by
   * orders of magnitude rather than by a margin.
   */
  private static final Duration CLAIM_BUDGET = Duration.ofSeconds(2);

  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("run-admission-load");

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void oneThousandRunsAdmitDispatchAndStayFairlyShared() throws Exception {
    var dataSource = new DriverManagerDataSource(
        DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
    var jdbc = new JdbcTemplate(dataSource);

    var tenants = new ArrayList<ResourceChainFixture.ResourceIds>();
    for (var index = 0; index < TENANTS; index++) {
      var ids = seedResourceChain();
      // Weight 1 for all but one: fairness has to be visible in the numbers, not
      // just asserted as a set membership.
      var weight = index == 0 ? 4 : 1;
      jdbc.update("""
          insert into tenant_run_quotas (tenant_id, max_active_runs, dispatch_weight)
          values (?, ?, ?)
          on conflict (tenant_id) do update
             set max_active_runs = excluded.max_active_runs,
                 dispatch_weight = excluded.dispatch_weight
          """, ids.tenantId(), TOTAL_RUNS, weight);
      tenants.add(ids);
    }

    var admitted = new AtomicInteger();
    var start = new CountDownLatch(1);
    var admissionStarted = Instant.now();
    try (var executor = Executors.newFixedThreadPool(WRITERS)) {
      var futures = new ArrayList<Future<?>>();
      for (var writer = 0; writer < WRITERS; writer++) {
        var slice = writer;
        futures.add(executor.submit(() -> {
          // One repository per writer, so they contend in the database rather
          // than sharing a transaction template.
          var repository = newRepository(dataSource);
          start.await();
          for (var n = slice; n < TOTAL_RUNS; n += WRITERS) {
            var ids = tenants.get(n % TENANTS);
            repository.save(ids.applicationId(), new Run(
                UUID.randomUUID(), ids.tenantId(), ids.sessionId(), ids.agentVersionId(),
                ids.workspaceId(), ids.modelPolicyId(), "load-" + n, "hello", RunStatus.QUEUED,
                1000, 100, 60, Instant.now()));
            admitted.incrementAndGet();
          }
          return null;
        }));
      }
      start.countDown();
      for (var future : futures) {
        future.get();
      }
    }
    var admissionElapsed = Duration.between(admissionStarted, Instant.now());

    // In-process counters can both be right while the database holds a
    // different number, which is the failure a quota bug actually produces.
    assertThat(admitted.get()).isEqualTo(TOTAL_RUNS);
    assertThat(jdbc.queryForObject("select count(*) from runs", Integer.class))
        .isEqualTo(TOTAL_RUNS);
    assertThat(jdbc.queryForObject(
        "select count(*) from outbox_events where published_at is null", Integer.class))
        .isEqualTo(TOTAL_RUNS);

    var outbox = new JdbcOutboxRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));

    // The first claim runs against the full backlog, which is the expensive
    // case and the one the fairness assertion needs.
    var firstClaimStarted = Instant.now();
    var firstBatch = outbox.claimNext(DRAIN_BATCH, UUID.randomUUID(), Duration.ofMinutes(5));
    var firstClaimElapsed = Duration.between(firstClaimStarted, Instant.now());

    assertThat(firstBatch).hasSize(DRAIN_BATCH);
    var firstBatchShares = countByTenant(firstBatch);
    // Under the old strict FIFO a batch of 100 drawn from a 1000-message backlog
    // could be a single tenant's. Every tenant appearing is the property that
    // changed.
    assertThat(firstBatchShares.keySet()).hasSize(TENANTS);
    var heavy = tenants.getFirst().tenantId();
    assertThat(firstBatchShares.get(heavy))
        .as("the tenant at weight 4 takes about four times a weight 1 share")
        .isGreaterThan(firstBatchShares.values().stream()
            .filter(share -> !share.equals(firstBatchShares.get(heavy)))
            .findFirst().orElseThrow() * 2);

    var claimTimings = new ArrayList<Duration>();
    claimTimings.add(firstClaimElapsed);
    var drained = firstBatch.size();
    while (drained < TOTAL_RUNS) {
      var claimStarted = Instant.now();
      var batch = outbox.claimNext(DRAIN_BATCH, UUID.randomUUID(), Duration.ofMinutes(5));
      claimTimings.add(Duration.between(claimStarted, Instant.now()));
      if (batch.isEmpty()) {
        break;
      }
      drained += batch.size();
    }

    assertThat(drained).isEqualTo(TOTAL_RUNS);
    var slowestClaim = claimTimings.stream().max(Duration::compareTo).orElseThrow();
    assertThat(slowestClaim)
        .as("a claim against a %d message backlog", TOTAL_RUNS)
        .isLessThan(CLAIM_BUDGET);

    System.out.printf(
        "load: %d runs admitted by %d writers in %d ms (%.0f runs/s); "
            + "drain %d claims of %d, slowest %d ms, first claim %d ms%n",
        TOTAL_RUNS, WRITERS, admissionElapsed.toMillis(),
        TOTAL_RUNS * 1000.0 / Math.max(1, admissionElapsed.toMillis()),
        claimTimings.size(), DRAIN_BATCH, slowestClaim.toMillis(), firstClaimElapsed.toMillis());
    System.out.printf("load: first batch shares %s%n", firstBatchShares);
  }

  private Map<UUID, Integer> countByTenant(List<OutboxMessage> batch) {
    var shares = new LinkedHashMap<UUID, Integer>();
    for (var message : batch) {
      shares.merge(message.tenantId(), 1, Integer::sum);
    }
    return shares;
  }

  private JdbcRunRepository newRepository(DataSource dataSource) {
    return new JdbcRunRepository(
        new JdbcTemplate(dataSource),
        new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
  }

  private ResourceChainFixture.ResourceIds seedResourceChain() throws Exception {
    return ResourceChainFixture.seed(DATABASE);
  }
}
