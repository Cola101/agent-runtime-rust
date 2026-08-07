package com.agentplatform.control.persistence;

import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.sql.Connection;
import java.sql.DriverManager;
import java.util.UUID;

/**
 * Seeds the tenant -> application -> project -> workspace -> agent -> session
 * chain a Run needs before it can be saved.
 *
 * <p>Shared rather than copied. Two copies drift, and the copy that drifts is
 * always the one whose test then fails for a reason that has nothing to do with
 * what it was written to check.
 */
final class ResourceChainFixture {
  private ResourceChainFixture() {}

  record ResourceIds(
      UUID tenantId,
      UUID applicationId,
      UUID workspaceId,
      UUID agentVersionId,
      UUID sessionId,
      UUID modelPolicyId) {}

  static ResourceIds seed(NativeDatabase database) throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    try (Connection connection = DriverManager.getConnection(
        database.jdbcUrl(), database.username(), database.password());
        var statement = connection.createStatement()) {
      statement.execute("select set_config('app.tenant_id','%s',false)".formatted(tenantId));
      statement.execute(
          "insert into tenants (tenant_id,id,slug,display_name) values ('%s','%s','t-%s','Tenant')"
              .formatted(tenantId, tenantId, tenantId));
      statement.execute("insert into applications (tenant_id,id,name) values ('%s','%s','App')"
          .formatted(tenantId, applicationId));
      statement.execute(
          "insert into projects (tenant_id,id,application_id,name) values ('%s','%s','%s','Project')"
              .formatted(tenantId, projectId, applicationId));
      statement.execute(
          "insert into workspaces (tenant_id,id,project_id,name) values ('%s','%s','%s','Workspace')"
              .formatted(tenantId, workspaceId, projectId));
      statement.execute(
          "insert into agents (tenant_id,id,workspace_id,name) values ('%s','%s','%s','Agent')"
              .formatted(tenantId, agentId, workspaceId));
      statement.execute(
          ("insert into agent_versions (tenant_id,id,application_id,agent_id,version,spec)"
              + " values ('%s','%s','%s','%s',1,'{}')")
              .formatted(tenantId, agentVersionId, applicationId, agentId));
      statement.execute("insert into sessions (tenant_id,id,workspace_id) values ('%s','%s','%s')"
          .formatted(tenantId, sessionId, workspaceId));
      statement.execute(
          ("insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id)"
              + " values ('%s','%s','%s','Default','{}','%s')")
              .formatted(tenantId, modelPolicyId, workspaceId, applicationId));
    }
    return new ResourceIds(
        tenantId, applicationId, workspaceId, agentVersionId, sessionId, modelPolicyId);
  }
}
