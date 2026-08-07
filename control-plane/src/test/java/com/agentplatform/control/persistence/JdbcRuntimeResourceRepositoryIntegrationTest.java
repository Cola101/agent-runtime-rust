package com.agentplatform.control.persistence;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.agentplatform.control.resource.ResourceConflict;
import com.agentplatform.control.resource.ResourceParentNotFound;
import com.agentplatform.control.resource.RuntimeResourceService;
import com.agentplatform.control.resource.SignedSkillArtifact;
import com.agentplatform.control.resource.SubagentRoleDefinition;
import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.transaction.support.TransactionTemplate;

class JdbcRuntimeResourceRepositoryIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("jdbc-runtime-resources");

  @BeforeAll
  static void startDatabase() {
    DATABASE.migrate();
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void createsAnApplicationScopedRunTargetFromIndependentResources() {
    var jdbc = new JdbcTemplate(DATABASE.dataSource());
    var transactions = new TransactionTemplate(
        new DataSourceTransactionManager(DATABASE.dataSource()));
    var tenantId = UUID.randomUUID();
    var applicationA = UUID.randomUUID();
    var applicationB = UUID.randomUUID();
    var projectA = seedTenant(jdbc, tenantId, applicationA, applicationB);
    var repository = new JdbcRuntimeResourceRepository(jdbc, transactions);
    var resources = new RuntimeResourceService(
        repository,
        (sealedTenant, provider, secret) -> """
            {"schema_version":1,"key_id":"test","algorithm":"test",
             "encrypted_key":"wrapped","nonce":"nonce","ciphertext":"ciphertext"}
            """,
        artifact -> new SignedSkillArtifact(
            "a".repeat(64), "test-skill-key", "A".repeat(86)));

    var context = resources.context(tenantId, applicationA);
    assertThat(context.applicationId()).isEqualTo(applicationA);
    assertThat(context.projects()).extracting("id").containsExactly(projectA);

    var workspace = resources.createWorkspace(
        tenantId, applicationA, projectA, "Release Workspace");
    var agent = resources.createAgent(
        tenantId, applicationA, workspace.id(), "Release Agent");
    var skill = resources.publishSkillVersion(
        tenantId, applicationA, "workspace-review", "1.0.0",
        "Review workspace evidence", "Read files before answering.",
        List.of("workspace.read_text"),
        List.of("darwin-arm64", "linux-arm64", "linux-x86_64"), "0.1.0");
    var versionOne = resources.createAgentVersion(
        tenantId, applicationA, agent.id(), "Review release changes.",
        List.of("tool:workspace.read"), List.of(skill.id()),
        List.of(new SubagentRoleDefinition(
            "reviewer", "Review evidence and report findings.",
            List.of("tool:workspace.read"))));
    var versionTwo = resources.createAgentVersion(
        tenantId, applicationA, agent.id(), "Review release changes carefully.", List.of());
    var provider = resources.createModelProvider(
        tenantId, applicationA, "Primary Provider", "openai_compatible",
        "https://models.example.test/v1/chat/completions", "test-model", "tenant-api-key");
    var modelPolicy = resources.createModelPolicy(
        tenantId, applicationA, workspace.id(), "Primary", "ordered_failover",
        List.of(provider.id()));
    var session = resources.createSession(
        tenantId, applicationA, workspace.id(), "Release review");

    assertThat(versionOne.version()).isEqualTo(1);
    assertThat(versionOne.skillVersionIds()).containsExactly(skill.id());
    assertThat(versionOne.subagentRoles()).extracting("name").containsExactly("reviewer");
    assertThat(versionTwo.version()).isEqualTo(2);
    assertThat(jdbc.queryForObject(
        "select spec->>'instructions' from agent_versions where tenant_id=? and id=?",
        String.class, tenantId, versionOne.id())).isEqualTo("Review release changes.");
    assertThat(jdbc.queryForObject(
        "select spec->'subagent_roles'->0->>'instructions' from agent_versions where tenant_id=? and id=?",
        String.class, tenantId, versionOne.id()))
        .isEqualTo("Review evidence and report findings.");
    assertThat(jdbc.queryForList("""
        select skill_version_id from agent_version_skills
         where tenant_id=? and agent_version_id=? order by ordinal
        """, UUID.class, tenantId, versionOne.id())).containsExactly(skill.id());
    assertThat(jdbc.queryForObject(
        "select title from sessions where tenant_id=? and id=?",
        String.class, tenantId, session.id())).isEqualTo("Release review");
    assertThat(provider.credentialStatus()).isEqualTo("configured");
    assertThat(jdbc.queryForObject(
        "select credential_envelope::text from model_providers where tenant_id=? and id=?",
        String.class, tenantId, provider.id())).doesNotContain("tenant-api-key");
    assertThat(jdbc.queryForList(
        "select provider_id from model_policy_candidates where tenant_id=? and model_policy_id=? order by priority",
        UUID.class, tenantId, modelPolicy.id())).containsExactly(provider.id());

    var targets = new JdbcRunTargetRepository(jdbc, transactions)
        .findAvailable(tenantId, applicationA, 100);
    assertThat(targets).hasSize(2);
    assertThat(targets).allSatisfy(target -> {
      assertThat(target.workspaceId()).isEqualTo(workspace.id());
      assertThat(target.agentName()).isEqualTo("Release Agent");
      assertThat(target.modelPolicyId()).isEqualTo(modelPolicy.id());
      assertThat(target.sessionId()).isEqualTo(session.id());
    });
  }

  // The service checks the name shape before the insert, but the service is not
  // the only way rows get written. The database constraint is the one that
  // cannot be bypassed, so it is checked where it lives.
  @Test
  void theDatabaseRefusesAnMcpServerNameThatCouldForgeAQualifiedToolName() {
    var jdbc = new JdbcTemplate(DATABASE.dataSource());
    var transactions = new TransactionTemplate(
        new DataSourceTransactionManager(DATABASE.dataSource()));
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    seedTenant(jdbc, tenantId, applicationId, UUID.randomUUID());
    var repository = new JdbcRuntimeResourceRepository(jdbc, transactions);

    var created = repository.createMcpServer(
        tenantId, applicationId, UUID.randomUUID(), "search",
        "https://mcp.example.com/rpc", null);
    assertThat(created.name()).isEqualTo("search");
    assertThat(created.credentialStatus())
        .as("no envelope was supplied, so nothing may claim one was sealed")
        .isEqualTo("absent");

    assertThatThrownBy(() -> repository.createMcpServer(
        tenantId, applicationId, UUID.randomUUID(), "other/tool",
        "https://mcp.example.com/rpc", null))
        .isInstanceOf(org.springframework.dao.DataIntegrityViolationException.class);
  }

  @Test
  void rejectsCrossApplicationParentsAndDuplicateNamesWithoutLeakingTheirExistence() {
    var jdbc = new JdbcTemplate(DATABASE.dataSource());
    var transactions = new TransactionTemplate(
        new DataSourceTransactionManager(DATABASE.dataSource()));
    var tenantId = UUID.randomUUID();
    var applicationA = UUID.randomUUID();
    var applicationB = UUID.randomUUID();
    var projectA = seedTenant(jdbc, tenantId, applicationA, applicationB);
    var resources = new RuntimeResourceService(
        new JdbcRuntimeResourceRepository(jdbc, transactions),
        (sealedTenant, provider, secret) -> "{}",
        artifact -> new SignedSkillArtifact(
            "b".repeat(64), "test-skill-key", "B".repeat(86)));

    assertThatThrownBy(() -> resources.createWorkspace(
        tenantId, applicationB, projectA, "Leaked Workspace"))
        .isInstanceOf(ResourceParentNotFound.class);

    resources.createWorkspace(tenantId, applicationA, projectA, "Unique Workspace");
    assertThatThrownBy(() -> resources.createWorkspace(
        tenantId, applicationA, projectA, "Unique Workspace"))
        .isInstanceOf(ResourceConflict.class);

    var providerA = resources.createModelProvider(
        tenantId, applicationA, "A Provider", "openai_compatible",
        "https://models.example.test/v1/chat/completions", "test-model", "secret");
    var workspaceB = resources.createWorkspace(
        tenantId, applicationB,
        jdbc.queryForObject(
            "select id from projects where tenant_id=? and application_id=?",
            UUID.class, tenantId, applicationB),
        "Application B Workspace");
    var skillA = resources.publishSkillVersion(
        tenantId, applicationA, "private-review", "1.0.0", "Private review",
        "Use the private review procedure.", List.of(), List.of("darwin-arm64"), "0.1.0");
    var agentB = resources.createAgent(
        tenantId, applicationB, workspaceB.id(), "Application B Agent");
    assertThatThrownBy(() -> resources.createAgentVersion(
        tenantId, applicationB, agentB.id(), "Do not leak Skill metadata.", List.of(),
        List.of(skillA.id())))
        .isInstanceOf(ResourceParentNotFound.class);
    assertThatThrownBy(() -> resources.createModelPolicy(
        tenantId, applicationB, workspaceB.id(), "Cross App", "ordered_failover",
        List.of(providerA.id())))
        .isInstanceOf(ResourceParentNotFound.class);
  }

  private UUID seedTenant(
      JdbcTemplate jdbc, UUID tenantId, UUID applicationA, UUID applicationB) {
    var projectA = UUID.randomUUID();
    var projectB = UUID.randomUUID();
    jdbc.queryForObject(
        "select set_config('app.tenant_id', ?, false)", String.class, tenantId.toString());
    jdbc.update("insert into tenants (tenant_id,id,slug,display_name) values (?,?,?,'Tenant')",
        tenantId, tenantId, "t-" + tenantId);
    jdbc.update("insert into applications (tenant_id,id,name) values (?,?,'Application A')",
        tenantId, applicationA);
    jdbc.update("insert into applications (tenant_id,id,name) values (?,?,'Application B')",
        tenantId, applicationB);
    jdbc.update("insert into projects (tenant_id,id,application_id,name) values (?,?,?,'Project A')",
        tenantId, projectA, applicationA);
    jdbc.update("insert into projects (tenant_id,id,application_id,name) values (?,?,?,'Project B')",
        tenantId, projectB, applicationB);
    return projectA;
  }
}
