package com.agentplatform.control.outbox;

import static org.assertj.core.api.Assertions.assertThat;

import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.time.Duration;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.jdbc.datasource.DriverManagerDataSource;
import org.springframework.transaction.support.TransactionTemplate;

class JdbcOutboxRepositoryIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("jdbc-outbox-repository");

  private static JdbcTemplate jdbc;
  private static JdbcOutboxRepository repository;

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
    var dataSource = DATABASE.dataSource();
    jdbc = new JdbcTemplate(dataSource);
    repository = new JdbcOutboxRepository(
        jdbc, new TransactionTemplate(new DataSourceTransactionManager(dataSource)));
  }

  // Fairness is a property of the whole claimable set, so a leftover message
  // from another test is not noise here -- it changes the answer.
  @BeforeEach
  void clearOutbox() {
    jdbc.update("delete from outbox_events");
    jdbc.update("delete from tenant_run_quotas");
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void activeClaimPreventsAnotherPublisherFromTakingTheSameMessage() {
    var messageId = insertOutboxMessage();
    var firstPublisher = UUID.randomUUID();

    var firstClaim = repository.claimNext(10, firstPublisher, Duration.ofSeconds(30));
    var competingClaim = repository.claimNext(10, UUID.randomUUID(), Duration.ofSeconds(30));

    assertThat(firstClaim).extracting(OutboxMessage::id).containsExactly(messageId);
    assertThat(firstClaim.getFirst().publishAttempts()).isOne();
    assertThat(competingClaim).isEmpty();
  }

  @Test
  void failedPublishReleasesClaimAndKeepsMessageRetryable() {
    var messageId = insertOutboxMessage();
    var firstPublisher = UUID.randomUUID();
    var firstClaim = repository.claimNext(1, firstPublisher, Duration.ofSeconds(30)).getFirst();

    assertThat(repository.release(
        firstClaim.tenantId(), firstClaim.id(), firstClaim.claimToken(), "broker unavailable"))
        .isTrue();
    var retry = repository.claimNext(1, UUID.randomUUID(), Duration.ofSeconds(30)).getFirst();

    assertThat(retry.id()).isEqualTo(messageId);
    assertThat(retry.publishAttempts()).isEqualTo(2);
    assertThat(jdbc.queryForObject(
        "select last_error from outbox_events where id = ?", String.class, messageId))
        .isEqualTo("broker unavailable");
  }

  @Test
  void onlyTheCurrentClaimCanMarkMessagePublished() {
    var messageId = insertOutboxMessage();
    var claim = repository.claimNext(1, UUID.randomUUID(), Duration.ofSeconds(30)).getFirst();

    assertThat(repository.markPublished(
        claim.tenantId(), claim.id(), UUID.randomUUID())).isFalse();
    assertThat(repository.markPublished(
        claim.tenantId(), claim.id(), claim.claimToken())).isTrue();
    assertThat(repository.claimNext(1, UUID.randomUUID(), Duration.ofSeconds(30))).isEmpty();
    assertThat(jdbc.queryForObject(
        "select published_at is not null from outbox_events where id = ?", Boolean.class, messageId))
        .isTrue();
  }

  // Strict FIFO lets one tenant's burst hold every other tenant behind it. The
  // small tenant here enqueues after the backlog exists, so under `order by
  // created_at` its message cannot appear until the backlog is fully drained.
  @Test
  void aTenantWithABacklogDoesNotDelayAnotherTenantsFirstMessage() {
    var noisy = UUID.randomUUID();
    var quiet = UUID.randomUUID();
    for (var i = 0; i < 50; i++) {
      insertOutboxMessage(noisy);
    }
    var quietMessage = insertOutboxMessage(quiet);

    var batch = repository.claimNext(10, UUID.randomUUID(), Duration.ofSeconds(30));

    assertThat(batch).extracting(OutboxMessage::id).contains(quietMessage);
  }

  // Fair does not mean equal. A tenant given four times the weight is served
  // four times the messages, and one given the default keeps a share rather than
  // being starved by the heavier tenant.
  @Test
  void dispatchWeightDividesTheBatchInProportion() {
    var heavy = UUID.randomUUID();
    var light = UUID.randomUUID();
    setDispatchWeight(heavy, 4);
    setDispatchWeight(light, 1);
    for (var i = 0; i < 20; i++) {
      insertOutboxMessage(light);
    }
    for (var i = 0; i < 20; i++) {
      insertOutboxMessage(heavy);
    }

    var batch = repository.claimNext(10, UUID.randomUUID(), Duration.ofSeconds(30));

    var heavyShare = batch.stream().filter(message -> message.tenantId().equals(heavy)).count();
    var lightShare = batch.stream().filter(message -> message.tenantId().equals(light)).count();
    assertThat(heavyShare + lightShare).isEqualTo(10);
    assertThat(heavyShare).isEqualTo(8);
    assertThat(lightShare).isEqualTo(2);
  }

  // A tenant that has never had a quota row -- which is every tenant before its
  // first Run -- must still be dispatched. Reading a missing weight as zero
  // would leave its messages unclaimable forever.
  @Test
  void aTenantWithNoQuotaRowIsStillDispatched() {
    // A configured tenant with a backlog has to be present, or a missing weight
    // could sort last and the test would pass anyway for want of a competitor.
    var configured = UUID.randomUUID();
    setDispatchWeight(configured, 1);
    for (var i = 0; i < 50; i++) {
      insertOutboxMessage(configured);
    }
    var unknown = UUID.randomUUID();
    var message = insertOutboxMessage(unknown);

    var batch = repository.claimNext(10, UUID.randomUUID(), Duration.ofSeconds(30));

    assertThat(batch).extracting(OutboxMessage::id).contains(message);
  }

  // Fairness reorders across tenants, never within one. A single tenant's own
  // messages must still drain oldest first, or an ordered event stream arrives
  // shuffled.
  @Test
  void messagesFromOneTenantKeepTheirOrder() {
    var tenantId = UUID.randomUUID();
    // Explicit ages rather than insertion order: `now()` is transaction start
    // time, and three round trips could in principle land in the same
    // microsecond, leaving the order decided by a random UUID.
    var first = insertOutboxMessage(tenantId, 5);
    var second = insertOutboxMessage(tenantId, 4);
    var third = insertOutboxMessage(tenantId, 3);
    insertOutboxMessage(tenantId, 2);
    insertOutboxMessage(tenantId, 1);

    // Fewer than are waiting: a batch big enough for all of them would take the
    // same set whichever way the rank ran, and the assertion would hold without
    // testing anything.
    var batch = repository.claimNext(3, UUID.randomUUID(), Duration.ofSeconds(30));

    assertThat(batch).extracting(OutboxMessage::id).containsExactly(first, second, third);
  }

  private void setDispatchWeight(UUID tenantId, int weight) {
    jdbc.update("""
        insert into tenant_run_quotas (tenant_id, max_active_runs, dispatch_weight)
        values (?, 64, ?)
        on conflict (tenant_id) do update set dispatch_weight = excluded.dispatch_weight
        """, tenantId, weight);
  }

  private UUID insertOutboxMessage(UUID tenantId) {
    return insertOutboxMessage(tenantId, 0);
  }

  private UUID insertOutboxMessage(UUID tenantId, int secondsAgo) {
    var messageId = UUID.randomUUID();
    jdbc.update("""
        insert into outbox_events (
          tenant_id, id, aggregate_type, aggregate_id, event_type, payload, created_at)
        values (?, ?, 'run', ?, 'run.queued', cast(? as jsonb),
                clock_timestamp() - (? * interval '1 second'))
        """, tenantId, messageId, UUID.randomUUID(),
        "{\"schema_version\":1,\"message_id\":\"" + messageId + "\"}", secondsAgo);
    return messageId;
  }

  private UUID insertOutboxMessage() {
    var tenantId = UUID.randomUUID();
    var messageId = UUID.randomUUID();
    jdbc.update("""
        insert into outbox_events (
          tenant_id, id, aggregate_type, aggregate_id, event_type, payload)
        values (?, ?, 'run', ?, 'run.queued', cast(? as jsonb))
        """, tenantId, messageId, UUID.randomUUID(),
        "{\"schema_version\":1,\"message_id\":\"" + messageId + "\"}");
    return messageId;
  }
}
