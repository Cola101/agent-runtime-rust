package com.agentplatform.control.persistence;

import com.agentplatform.control.resource.AgentResource;
import com.agentplatform.control.resource.AgentVersionResource;
import com.agentplatform.control.resource.ModelPolicyResource;
import com.agentplatform.control.resource.ModelProviderResource;
import com.agentplatform.control.resource.ProjectSummary;
import com.agentplatform.control.resource.ResourceConflict;
import com.agentplatform.control.resource.ResourceContext;
import com.agentplatform.control.resource.ResourceParentNotFound;
import com.agentplatform.control.resource.RuntimeResourceRepository;
import com.agentplatform.control.resource.SessionResource;
import com.agentplatform.control.resource.SignedSkillArtifact;
import com.agentplatform.control.resource.SkillArtifact;
import com.agentplatform.control.resource.SkillVersionResource;
import com.agentplatform.control.resource.SubagentRoleDefinition;
import com.agentplatform.control.resource.WorkspaceResource;
import java.util.List;
import java.util.UUID;
import org.springframework.dao.DuplicateKeyException;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.support.TransactionTemplate;

@Repository
public class JdbcRuntimeResourceRepository implements RuntimeResourceRepository {
  private final JdbcTemplate jdbc;
  private final TransactionTemplate transactions;

  public JdbcRuntimeResourceRepository(JdbcTemplate jdbc, TransactionTemplate transactions) {
    this.jdbc = jdbc;
    this.transactions = transactions;
  }

  @Override
  public ResourceContext findContext(UUID tenantId, UUID applicationId) {
    return transactions.execute(status -> {
      setTenant(tenantId);
      var applications = jdbc.query(
          "select name from applications where tenant_id = ? and id = ?",
          (row, rowNumber) -> row.getString("name"), tenantId, applicationId);
      if (applications.isEmpty()) throw new ResourceParentNotFound();
      var projects = jdbc.query("""
          select id,name from projects
           where tenant_id = ? and application_id = ?
           order by name,id limit 100
          """, (row, rowNumber) -> new ProjectSummary(
              row.getObject("id", UUID.class), row.getString("name")),
          tenantId, applicationId);
      return new ResourceContext(applicationId, applications.getFirst(), projects);
    });
  }

  @Override
  public WorkspaceResource createWorkspace(
      UUID tenantId, UUID applicationId, UUID projectId, String name) {
    return inTransaction(tenantId, () -> oneOrNotFound(jdbc.query("""
        insert into workspaces (tenant_id,id,project_id,name)
        select p.tenant_id,?,p.id,?
          from projects p
         where p.tenant_id = ? and p.application_id = ? and p.id = ?
        returning id,project_id,name,state,created_at
        """, (row, rowNumber) -> new WorkspaceResource(
            row.getObject("id", UUID.class), row.getObject("project_id", UUID.class),
            row.getString("name"), row.getString("state"),
            row.getTimestamp("created_at").toInstant()),
        UUID.randomUUID(), name, tenantId, applicationId, projectId)));
  }

  @Override
  public AgentResource createAgent(
      UUID tenantId, UUID applicationId, UUID workspaceId, String name) {
    return inTransaction(tenantId, () -> oneOrNotFound(jdbc.query("""
        insert into agents (tenant_id,id,workspace_id,name)
        select w.tenant_id,?,w.id,?
          from workspaces w
          join projects p on p.tenant_id = w.tenant_id and p.id = w.project_id
         where w.tenant_id = ? and p.application_id = ? and w.id = ? and w.state <> 'deleted'
        returning id,workspace_id,name,created_at
        """, (row, rowNumber) -> new AgentResource(
            row.getObject("id", UUID.class), row.getObject("workspace_id", UUID.class),
            row.getString("name"), row.getTimestamp("created_at").toInstant()),
        UUID.randomUUID(), name, tenantId, applicationId, workspaceId)));
  }

  @Override
  public AgentVersionResource createAgentVersion(
      UUID tenantId,
      UUID applicationId,
      UUID agentId,
      String instructions,
      List<String> delegatedScopes,
      List<UUID> skillVersionIds,
      List<SubagentRoleDefinition> subagentRoles) {
    return inTransaction(tenantId, () -> {
      var authorized = jdbc.query("""
          select a.id
            from agents a
            join workspaces w on w.tenant_id = a.tenant_id and w.id = a.workspace_id
            join projects p on p.tenant_id = w.tenant_id and p.id = w.project_id
           where a.tenant_id = ? and p.application_id = ? and a.id = ? and w.state <> 'deleted'
           for update of a
          """, (row, rowNumber) -> row.getObject("id", UUID.class),
          tenantId, applicationId, agentId);
      if (authorized.isEmpty()) throw new ResourceParentNotFound();
      var version = jdbc.queryForObject(
          "select coalesce(max(version),0) + 1 from agent_versions where tenant_id=? and agent_id=?",
          Integer.class, tenantId, agentId);
      var id = UUID.randomUUID();
      var createdAt = jdbc.queryForObject("""
          insert into agent_versions (tenant_id,id,agent_id,application_id,version,spec)
          values (?,?,?,?,?,jsonb_build_object(
            'instructions',?,'delegated_scopes',to_jsonb(?::text[]),
            'subagent_roles',?::jsonb))
          returning created_at
          """, (row, rowNumber) -> row.getTimestamp("created_at").toInstant(),
          tenantId, id, agentId, applicationId, version, instructions,
          delegatedScopes.toArray(String[]::new), subagentRolesJson(subagentRoles));
      for (var ordinal = 0; ordinal < skillVersionIds.size(); ordinal++) {
        var inserted = jdbc.update("""
            insert into agent_version_skills (
              tenant_id,application_id,agent_version_id,ordinal,skill_version_id,artifact_digest)
            select sv.tenant_id,sv.application_id,?,?,sv.id,sv.artifact_digest
              from skill_versions sv
             where sv.tenant_id=? and sv.application_id=? and sv.id=?
            """, id, ordinal, tenantId, applicationId, skillVersionIds.get(ordinal));
        if (inserted != 1) throw new ResourceParentNotFound();
      }
      return new AgentVersionResource(
          id, agentId, version, instructions, delegatedScopes, skillVersionIds, subagentRoles,
          createdAt);
    });
  }

  private String subagentRolesJson(List<SubagentRoleDefinition> roles) {
    var result = new com.fasterxml.jackson.databind.ObjectMapper().createArrayNode();
    for (var role : roles) {
      var item = result.addObject();
      item.put("name", role.name());
      item.put("instructions", role.instructions());
      var scopes = item.putArray("delegated_scopes");
      role.delegatedScopes().forEach(scopes::add);
    }
    return result.toString();
  }

  @Override
  public SkillVersionResource publishSkillVersion(
      SkillArtifact artifact, SignedSkillArtifact signedArtifact) {
    return inTransaction(artifact.tenantId(), () -> {
      var skillId = jdbc.query("""
          insert into skills (tenant_id,id,application_id,name)
          select a.tenant_id,?,a.id,?
            from applications a
           where a.tenant_id=? and a.id=?
          on conflict (tenant_id,application_id,name) do update set name=skills.name
          returning id
          """, (row, rowNumber) -> row.getObject("id", UUID.class),
          UUID.randomUUID(), artifact.name(), artifact.tenantId(), artifact.applicationId());
      if (skillId.isEmpty()) throw new ResourceParentNotFound();
      var artifactJson = new com.fasterxml.jackson.databind.ObjectMapper().createObjectNode();
      artifactJson.put("schema_version", artifact.schemaVersion());
      artifactJson.put("tenant_id", artifact.tenantId().toString());
      artifactJson.put("application_id", artifact.applicationId().toString());
      artifactJson.put("skill_version_id", artifact.skillVersionId().toString());
      artifactJson.put("name", artifact.name());
      artifactJson.put("semantic_version", artifact.semanticVersion());
      artifactJson.put("description", artifact.description());
      artifactJson.put("instructions", artifact.instructions());
      var tools = artifactJson.putArray("tool_names");
      artifact.toolNames().forEach(tools::add);
      var platforms = artifactJson.putArray("supported_platforms");
      artifact.supportedPlatforms().forEach(platforms::add);
      artifactJson.put("min_runtime_version", artifact.minRuntimeVersion());
      return jdbc.queryForObject("""
          insert into skill_versions (
            tenant_id,id,application_id,skill_id,semantic_version,artifact,artifact_digest,
            signing_key_id,signature)
          values (?,?,?,?,?,?::jsonb,?,?,?)
          returning created_at
          """, (row, rowNumber) -> new SkillVersionResource(
              artifact.skillVersionId(), skillId.getFirst(), artifact.applicationId(),
              artifact.name(), artifact.semanticVersion(), artifact.description(),
              artifact.instructions(), artifact.toolNames(), artifact.supportedPlatforms(),
              artifact.minRuntimeVersion(), signedArtifact.artifactDigest(),
              signedArtifact.signingKeyId(), signedArtifact.signature(),
              row.getTimestamp("created_at").toInstant()),
          artifact.tenantId(), artifact.skillVersionId(), artifact.applicationId(),
          skillId.getFirst(), artifact.semanticVersion(), artifactJson.toString(),
          signedArtifact.artifactDigest(), signedArtifact.signingKeyId(),
          signedArtifact.signature());
    });
  }

  @Override
  public ModelProviderResource createModelProvider(
      UUID tenantId,
      UUID applicationId,
      UUID providerId,
      String name,
      String protocol,
      String endpoint,
      String model,
      String credentialEnvelope) {
    return inTransaction(tenantId, () -> oneOrNotFound(jdbc.query("""
        insert into model_providers (
          tenant_id,id,application_id,name,protocol,endpoint,model,credential_envelope)
        select a.tenant_id,?,a.id,?,?,?,?,?::jsonb
          from applications a
         where a.tenant_id = ? and a.id = ?
        returning id,name,protocol,endpoint,model,state,created_at
        """, (row, rowNumber) -> new ModelProviderResource(
            row.getObject("id", UUID.class), row.getString("name"),
            row.getString("protocol"), row.getString("endpoint"), row.getString("model"),
            row.getString("state"), "configured", row.getTimestamp("created_at").toInstant()),
        providerId, name, protocol, endpoint, model, credentialEnvelope,
        tenantId, applicationId)));
  }

  @Override
  public ModelPolicyResource createModelPolicy(
      UUID tenantId,
      UUID applicationId,
      UUID workspaceId,
      String name,
      String routing,
      List<UUID> providerIds) {
    return inTransaction(tenantId, () -> {
      var created = oneOrNotFound(jdbc.query("""
        insert into model_policies (tenant_id,id,workspace_id,name,policy,application_id)
        select w.tenant_id,?,w.id,?,jsonb_build_object('routing',?),p.application_id
          from workspaces w
          join projects p on p.tenant_id = w.tenant_id and p.id = w.project_id
         where w.tenant_id = ? and p.application_id = ? and w.id = ? and w.state <> 'deleted'
        returning id,workspace_id,name,policy->>'routing' as routing,created_at
        """, (row, rowNumber) -> new ModelPolicyResource(
            row.getObject("id", UUID.class), row.getObject("workspace_id", UUID.class),
            row.getString("name"), row.getString("routing"), providerIds,
            row.getTimestamp("created_at").toInstant()),
        UUID.randomUUID(), name, routing, tenantId, applicationId, workspaceId));
      for (var priority = 0; priority < providerIds.size(); priority++) {
        var inserted = jdbc.update("""
            insert into model_policy_candidates (
              tenant_id,application_id,model_policy_id,provider_id,priority)
            select p.tenant_id,p.application_id,?,p.id,?
              from model_providers p
             where p.tenant_id = ? and p.application_id = ? and p.id = ? and p.state = 'active'
            """, created.id(), priority, tenantId, applicationId, providerIds.get(priority));
        if (inserted != 1) throw new ResourceParentNotFound();
      }
      return created;
    });
  }

  @Override
  public SessionResource createSession(
      UUID tenantId, UUID applicationId, UUID workspaceId, String title) {
    return inTransaction(tenantId, () -> oneOrNotFound(jdbc.query("""
        insert into sessions (tenant_id,id,workspace_id,title)
        select w.tenant_id,?,w.id,?
          from workspaces w
          join projects p on p.tenant_id = w.tenant_id and p.id = w.project_id
         where w.tenant_id = ? and p.application_id = ? and w.id = ? and w.state <> 'deleted'
        returning id,workspace_id,title,state,created_at
        """, (row, rowNumber) -> new SessionResource(
            row.getObject("id", UUID.class), row.getObject("workspace_id", UUID.class),
            row.getString("title"), row.getString("state"),
            row.getTimestamp("created_at").toInstant()),
        UUID.randomUUID(), title, tenantId, applicationId, workspaceId)));
  }

  private <T> T inTransaction(UUID tenantId, Operation<T> operation) {
    try {
      return transactions.execute(status -> {
        setTenant(tenantId);
        return operation.run();
      });
    } catch (DuplicateKeyException duplicate) {
      throw new ResourceConflict();
    }
  }

  private <T> T oneOrNotFound(List<T> values) {
    if (values.isEmpty()) throw new ResourceParentNotFound();
    return values.getFirst();
  }

  private void setTenant(UUID tenantId) {
    jdbc.queryForObject(
        "select set_config('app.tenant_id', ?, true)", String.class, tenantId.toString());
  }

  @FunctionalInterface
  private interface Operation<T> {
    T run();
  }
}
