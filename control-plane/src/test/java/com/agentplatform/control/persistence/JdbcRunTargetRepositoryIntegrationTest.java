package com.agentplatform.control.persistence;

import static org.assertj.core.api.Assertions.assertThat;

import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.transaction.support.TransactionTemplate;

class JdbcRunTargetRepositoryIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("jdbc-run-targets");

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void applicationTargetQueryDoesNotExposeAnotherApplicationInTheSameTenant() {
    var jdbc = new JdbcTemplate(DATABASE.dataSource());
    var transactions = new TransactionTemplate(
        new DataSourceTransactionManager(DATABASE.dataSource()));
    var tenantId = UUID.randomUUID();
    var applicationA = UUID.randomUUID();
    var applicationB = UUID.randomUUID();
    seedTenant(jdbc, tenantId, applicationA, applicationB);
    var targetA = seedTarget(jdbc, tenantId, applicationA, "Workspace A", "Agent A");
    seedTarget(jdbc, tenantId, applicationB, "Workspace B", "Agent B");
    var repository = new JdbcRunTargetRepository(jdbc, transactions);

    var targets = repository.findAvailable(tenantId, applicationA, 100);

    assertThat(targets).containsExactly(targetA);
  }

  private void seedTenant(
      JdbcTemplate jdbc, UUID tenantId, UUID applicationA, UUID applicationB) {
    jdbc.queryForObject(
        "select set_config('app.tenant_id', ?, false)", String.class, tenantId.toString());
    jdbc.update("insert into tenants (tenant_id,id,slug,display_name) values (?,?,?,'Tenant')",
        tenantId, tenantId, "t-" + tenantId);
    jdbc.update("insert into applications (tenant_id,id,name) values (?,?,'Application A')",
        tenantId, applicationA);
    jdbc.update("insert into applications (tenant_id,id,name) values (?,?,'Application B')",
        tenantId, applicationB);
  }

  private com.agentplatform.control.run.RunTarget seedTarget(
      JdbcTemplate jdbc, UUID tenantId, UUID applicationId, String workspaceName, String agentName) {
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    jdbc.update("insert into projects (tenant_id,id,application_id,name) values (?,?,?,'Project')",
        tenantId, projectId, applicationId);
    jdbc.update("insert into workspaces (tenant_id,id,project_id,name) values (?,?,?,?)",
        tenantId, workspaceId, projectId, workspaceName);
    jdbc.update("insert into agents (tenant_id,id,workspace_id,name) values (?,?,?,?)",
        tenantId, agentId, workspaceId, agentName);
    jdbc.update("insert into agent_versions (tenant_id,id,application_id,agent_id,version,spec) values (?,?,?,?,1,'{}')",
        tenantId, agentVersionId, applicationId, agentId);
    jdbc.update("insert into sessions (tenant_id,id,workspace_id) values (?,?,?)",
        tenantId, sessionId, workspaceId);
    jdbc.update("insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id) values (?,?,?,'Default','{}',?)",
        tenantId, modelPolicyId, workspaceId, applicationId);
    return new com.agentplatform.control.run.RunTarget(
        sessionId, workspaceId, workspaceName, agentVersionId, agentName, 1,
        modelPolicyId, "Default");
  }
}
