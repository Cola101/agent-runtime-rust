package com.agentplatform.control.outbox;

import static org.assertj.core.api.Assertions.assertThat;

import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.time.Duration;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
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
