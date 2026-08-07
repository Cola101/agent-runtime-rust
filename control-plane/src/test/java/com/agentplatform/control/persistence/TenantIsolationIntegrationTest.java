package com.agentplatform.control.persistence;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.SQLException;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

class TenantIsolationIntegrationTest {
  private static final String APP_USER = "runtime_test";
  private static final String APP_PASSWORD = "runtime_test_password";
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("tenant-isolation");

  @BeforeAll
  static void migrateDatabase() {
    DATABASE.migrate();
    try (var connection = DriverManager.getConnection(
            DATABASE.jdbcUrl(), DATABASE.username(), DATABASE.password());
        var statement = connection.createStatement()) {
      statement.execute("create role " + APP_USER + " login password '" + APP_PASSWORD + "'");
      statement.execute("grant usage on schema public to " + APP_USER);
      statement.execute("grant select, insert, update, delete on all tables in schema public to " + APP_USER);
    } catch (SQLException exception) {
      throw new IllegalStateException("failed to configure non-owner runtime database role", exception);
    }
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void rowLevelSecurityHidesOtherTenantRuns() throws Exception {
    var tenantA = UUID.randomUUID();
    var tenantB = UUID.randomUUID();
    try (var connection = connection()) {
      insertTenant(connection, tenantA, "tenant-a");
      insertTenant(connection, tenantB, "tenant-b");
      insertResourceChainAndRun(connection, tenantA);
      insertResourceChainAndRun(connection, tenantB);

      setTenant(connection, tenantA);
      try (var statement = connection.createStatement();
          var rows = statement.executeQuery("select tenant_id from runs")) {
        assertThat(rows.next()).isTrue();
        assertThat(rows.getObject(1, UUID.class)).isEqualTo(tenantA);
        assertThat(rows.next()).isFalse();
      }
    }
  }

  @Test
  void compositeForeignKeysRejectCrossTenantReferences() throws Exception {
    var tenantA = UUID.randomUUID();
    var tenantB = UUID.randomUUID();
    var applicationB = UUID.randomUUID();
    try (var connection = connection()) {
      insertTenant(connection, tenantA, "tenant-a-cross");
      insertTenant(connection, tenantB, "tenant-b-cross");
      setTenant(connection, tenantB);
      execute(connection, "insert into applications (tenant_id, id, name) values ('%s','%s','app-b')"
          .formatted(tenantB, applicationB));

      setTenant(connection, tenantA);
      assertThatThrownBy(() -> execute(connection,
          "insert into projects (tenant_id, id, application_id, name) values ('%s','%s','%s','invalid')"
              .formatted(tenantA, UUID.randomUUID(), applicationB)))
          .isInstanceOf(SQLException.class)
          .hasMessageContaining("projects_tenant_application_fk");
    }
  }

  @Test
  void rowLevelSecurityHidesOtherTenantSessionToolGrants() throws Exception {
    var tenantA = UUID.randomUUID();
    var tenantB = UUID.randomUUID();
    try (var connection = connection()) {
      insertTenant(connection, tenantA, "tenant-a-grants");
      insertTenant(connection, tenantB, "tenant-b-grants");
      var chainA = insertResourceChainAndRun(connection, tenantA);
      var chainB = insertResourceChainAndRun(connection, tenantB);
      insertSessionToolGrant(connection, tenantA, chainA);
      insertSessionToolGrant(connection, tenantB, chainB);

      setTenant(connection, tenantA);
      try (var statement = connection.createStatement();
          var rows = statement.executeQuery("select tenant_id from session_tool_grants")) {
        assertThat(rows.next()).isTrue();
        assertThat(rows.getObject(1, UUID.class)).isEqualTo(tenantA);
        assertThat(rows.next()).isFalse();
      }
    }
  }

  private static Connection connection() throws SQLException {
    return DriverManager.getConnection(DATABASE.jdbcUrl(), APP_USER, APP_PASSWORD);
  }

  private static void insertTenant(Connection connection, UUID tenantId, String slug) throws SQLException {
    setTenant(connection, tenantId);
    execute(connection, "insert into tenants (tenant_id, id, slug, display_name) values ('%s','%s','%s','%s')"
        .formatted(tenantId, tenantId, slug, slug));
  }

  private static ResourceChain insertResourceChainAndRun(
      Connection connection, UUID tenantId) throws SQLException {
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    setTenant(connection, tenantId);
    execute(connection, "insert into applications (tenant_id,id,name) values ('%s','%s','app')"
        .formatted(tenantId, applicationId));
    execute(connection, "insert into projects (tenant_id,id,application_id,name) values ('%s','%s','%s','project')"
        .formatted(tenantId, projectId, applicationId));
    execute(connection, "insert into workspaces (tenant_id,id,project_id,name) values ('%s','%s','%s','workspace')"
        .formatted(tenantId, workspaceId, projectId));
    execute(connection, "insert into agents (tenant_id,id,workspace_id,name) values ('%s','%s','%s','agent')"
        .formatted(tenantId, agentId, workspaceId));
    execute(connection, "insert into agent_versions (tenant_id,id,application_id,agent_id,version,spec) values ('%s','%s','%s','%s',1,'{}')"
        .formatted(tenantId, agentVersionId, applicationId, agentId));
    execute(connection, "insert into sessions (tenant_id,id,workspace_id) values ('%s','%s','%s')"
        .formatted(tenantId, sessionId, workspaceId));
    execute(connection, "insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id) values ('%s','%s','%s','default','{}','%s')"
        .formatted(tenantId, modelPolicyId, workspaceId, applicationId));
    execute(connection, "insert into runs (tenant_id,application_id,id,session_id,workspace_id,agent_version_id,model_policy_id,idempotency_key,input,status,max_tokens,max_cost_cents,max_duration_seconds) values ('%s','%s','%s','%s','%s','%s','%s','key','hello','queued',1000,100,60)"
        .formatted(tenantId, applicationId, runId, sessionId, workspaceId,
            agentVersionId, modelPolicyId));
    return new ResourceChain(applicationId, workspaceId, agentVersionId, sessionId, runId);
  }

  private static void insertSessionToolGrant(
      Connection connection, UUID tenantId, ResourceChain chain) throws SQLException {
    setTenant(connection, tenantId);
    var approvalId = UUID.randomUUID();
    execute(connection, "insert into approvals (tenant_id,id,run_id,request) values ('%s','%s','%s','{}')"
        .formatted(tenantId, approvalId, chain.runId()));
    execute(connection, """
        insert into session_tool_grants (
          tenant_id,id,source_run_id,application_id,session_id,workspace_id,agent_version_id,
          scope_digest,policy_digest,policy_snapshot,tool_name,effect,sandbox,
          source_approval_id,created_by)
        values ('%s','%s','%s','%s','%s','%s','%s','%s','%s','{}',
                'workspace.read_text','pure','trusted_native','%s','reviewer')
        """.formatted(
            tenantId, UUID.randomUUID(), chain.runId(), chain.applicationId(), chain.sessionId(),
            chain.workspaceId(), chain.agentVersionId(), "a".repeat(64), "b".repeat(64),
            approvalId));
  }

  private static void setTenant(Connection connection, UUID tenantId) throws SQLException {
    execute(connection, "select set_config('app.tenant_id','%s',false)".formatted(tenantId));
  }

  private static void execute(Connection connection, String sql) throws SQLException {
    try (var statement = connection.createStatement()) {
      statement.execute(sql);
    }
  }

  private record ResourceChain(
      UUID applicationId, UUID workspaceId, UUID agentVersionId, UUID sessionId, UUID runId) {}
}
