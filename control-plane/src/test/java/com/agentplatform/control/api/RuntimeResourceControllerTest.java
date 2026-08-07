package com.agentplatform.control.api;

import static org.mockito.Mockito.when;
import static org.springframework.security.test.web.servlet.request.SecurityMockMvcRequestPostProcessors.jwt;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.post;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.header;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import com.agentplatform.control.resource.AgentResource;
import com.agentplatform.control.resource.AgentVersionResource;
import com.agentplatform.control.resource.ModelPolicyResource;
import com.agentplatform.control.resource.ModelProviderResource;
import com.agentplatform.control.resource.ProjectSummary;
import com.agentplatform.control.resource.ResourceContext;
import com.agentplatform.control.resource.RuntimeResourceService;
import com.agentplatform.control.resource.SessionResource;
import com.agentplatform.control.resource.SkillVersionResource;
import com.agentplatform.control.resource.SubagentRoleDefinition;
import com.agentplatform.control.resource.WorkspaceResource;
import com.agentplatform.control.security.SecurityConfiguration;
import java.time.Instant;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.context.annotation.Import;
import org.springframework.http.MediaType;
import org.springframework.security.oauth2.jwt.JwtDecoder;
import org.springframework.test.context.TestPropertySource;
import org.springframework.test.web.servlet.MockMvc;

@WebMvcTest(RuntimeResourceController.class)
@Import(SecurityConfiguration.class)
@TestPropertySource(properties = "spring.security.user.password=test-scrape-password")
class RuntimeResourceControllerTest {
  @Autowired private MockMvc mvc;
  @MockBean private RuntimeResourceService resources;
  @MockBean private JwtDecoder jwtDecoder;

  @Test
  void resourceReaderReceivesOnlyTheClaimedApplicationContext() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    when(resources.context(tenantId, applicationId))
        .thenReturn(new ResourceContext(applicationId, "Runtime App",
            List.of(new ProjectSummary(projectId, "Customer Project"))));

    mvc.perform(get("/v1/console/resource-context")
            .with(jwt().jwt(jwt -> jwt.subject("operator")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "resources:read"))))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.application_id").value(applicationId.toString()))
        .andExpect(jsonPath("$.application_name").value("Runtime App"))
        .andExpect(jsonPath("$.projects[0].id").value(projectId.toString()))
        .andExpect(jsonPath("$.tenant_id").doesNotExist());
  }

  @Test
  void runWriterCannotCreateConfigurationResources() throws Exception {
    mvc.perform(post("/v1/workspaces")
            .with(jwt().jwt(jwt -> jwt
                .claim("scope", "runs:write")))
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"project_id\":\"%s\",\"name\":\"Workspace\"}"
                .formatted(UUID.randomUUID())))
        .andExpect(status().isForbidden());
  }

  @Test
  void resourceWriterCreatesEveryRuntimeConfigurationResource() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var projectId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var agentId = UUID.randomUUID();
    var versionId = UUID.randomUUID();
    var skillId = UUID.randomUUID();
    var skillVersionId = UUID.randomUUID();
    var policyId = UUID.randomUUID();
    var providerId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var createdAt = Instant.parse("2026-08-02T01:02:03Z");
    when(resources.createWorkspace(tenantId, applicationId, projectId, "Release Workspace"))
        .thenReturn(new WorkspaceResource(
            workspaceId, projectId, "Release Workspace", "ready", createdAt));
    when(resources.createAgent(tenantId, applicationId, workspaceId, "Release Agent"))
        .thenReturn(new AgentResource(agentId, workspaceId, "Release Agent", createdAt));
    when(resources.publishSkillVersion(
        tenantId, applicationId, "workspace-review", "1.0.0", "Review workspace evidence",
        "Read files before answering.", List.of("workspace.read_text"),
        List.of("darwin-arm64", "linux-arm64", "linux-x86_64"), "0.1.0"))
        .thenReturn(new SkillVersionResource(
            skillVersionId, skillId, applicationId, "workspace-review", "1.0.0",
            "Review workspace evidence", "Read files before answering.",
            List.of("workspace.read_text"),
            List.of("darwin-arm64", "linux-arm64", "linux-x86_64"), "0.1.0",
            "a".repeat(64), "local-skill-key", "c2lnbmF0dXJl", createdAt));
    when(resources.createAgentVersion(
        tenantId, applicationId, agentId, "Review release changes.",
        List.of("tool:workspace.read"), List.of(skillVersionId),
        List.of(new SubagentRoleDefinition(
            "reviewer", "Review evidence and report findings.",
            List.of("tool:workspace.read"))),
        java.util.Map.of()))
        .thenReturn(new AgentVersionResource(
            versionId, agentId, 1, "Review release changes.",
            List.of("tool:workspace.read"), List.of(skillVersionId),
            List.of(new SubagentRoleDefinition(
                "reviewer", "Review evidence and report findings.",
                List.of("tool:workspace.read"))), createdAt));
    when(resources.createModelProvider(
        tenantId, applicationId, "Primary Provider", "openai_compatible",
        "https://models.example.test/v1/chat/completions", "test-model", "tenant-api-key"))
        .thenReturn(new ModelProviderResource(
            providerId, "Primary Provider", "openai_compatible",
            "https://models.example.test/v1/chat/completions", "test-model",
            "active", "configured", createdAt));
    when(resources.createModelPolicy(
        tenantId, applicationId, workspaceId, "Primary", "ordered_failover",
        List.of(providerId)))
        .thenReturn(new ModelPolicyResource(
            policyId, workspaceId, "Primary", "ordered_failover",
            List.of(providerId), createdAt));
    when(resources.createSession(tenantId, applicationId, workspaceId, "Release review"))
        .thenReturn(new SessionResource(
            sessionId, workspaceId, "Release review", "active", createdAt));

    var authorization = jwt().jwt(jwt -> jwt.subject("operator")
        .claim("tenant_id", tenantId.toString())
        .claim("application_id", applicationId.toString())
        .claim("scope", "resources:write"));

    mvc.perform(post("/v1/workspaces").with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"project_id\":\"%s\",\"name\":\"Release Workspace\"}"
                .formatted(projectId)))
        .andExpect(status().isCreated())
        .andExpect(header().string("Location", "/v1/workspaces/" + workspaceId))
        .andExpect(jsonPath("$.id").value(workspaceId.toString()))
        .andExpect(jsonPath("$.project_id").value(projectId.toString()))
        .andExpect(jsonPath("$.state").value("ready"));

    mvc.perform(post("/v1/agents").with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"workspace_id\":\"%s\",\"name\":\"Release Agent\"}"
                .formatted(workspaceId)))
        .andExpect(status().isCreated())
        .andExpect(header().string("Location", "/v1/agents/" + agentId));

    mvc.perform(post("/v1/skills:publish").with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"name":"workspace-review","semantic_version":"1.0.0",
                 "description":"Review workspace evidence",
                 "instructions":"Read files before answering.",
                 "tool_names":["workspace.read_text"],
                 "supported_platforms":["darwin-arm64","linux-arm64","linux-x86_64"],
                 "min_runtime_version":"0.1.0"}
                """))
        .andExpect(status().isCreated())
        .andExpect(header().string("Location", "/v1/skill-versions/" + skillVersionId))
        .andExpect(jsonPath("$.id").value(skillVersionId.toString()))
        .andExpect(jsonPath("$.artifact_digest").value("a".repeat(64)))
        .andExpect(jsonPath("$.signing_key_id").value("local-skill-key"));

    mvc.perform(post("/v1/agents/{agentId}/versions", agentId).with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"instructions":"Review release changes.",
                 "delegated_scopes":["tool:workspace.read"],
                 "skill_version_ids":["%s"],
                 "subagent_roles":[{"name":"reviewer",
                   "instructions":"Review evidence and report findings.",
                   "delegated_scopes":["tool:workspace.read"]}]}
                """.formatted(skillVersionId)))
        .andExpect(status().isCreated())
        .andExpect(header().string(
            "Location", "/v1/agents/" + agentId + "/versions/" + versionId))
        .andExpect(jsonPath("$.version").value(1))
        .andExpect(jsonPath("$.instructions").value("Review release changes."))
        .andExpect(jsonPath("$.skill_version_ids[0]").value(skillVersionId.toString()))
        .andExpect(jsonPath("$.subagent_roles[0].name").value("reviewer"))
        .andExpect(jsonPath("$.subagent_roles[0].delegated_scopes[0]")
            .value("tool:workspace.read"));

    mvc.perform(post("/v1/model-providers").with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"name":"Primary Provider","protocol":"openai_compatible",
                 "endpoint":"https://models.example.test/v1/chat/completions",
                 "model":"test-model","api_key":"tenant-api-key"}
                """))
        .andExpect(status().isCreated())
        .andExpect(header().string("Location", "/v1/model-providers/" + providerId))
        .andExpect(jsonPath("$.credential_status").value("configured"))
        .andExpect(jsonPath("$.api_key").doesNotExist())
        .andExpect(jsonPath("$.credential_envelope").doesNotExist());

    mvc.perform(post("/v1/model-policies").with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"workspace_id":"%s","name":"Primary","routing":"ordered_failover",
                 "provider_ids":["%s"]}
                """.formatted(workspaceId, providerId)))
        .andExpect(status().isCreated())
        .andExpect(header().string("Location", "/v1/model-policies/" + policyId))
        .andExpect(jsonPath("$.provider_ids[0]").value(providerId.toString()));

    mvc.perform(post("/v1/sessions").with(authorization)
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"workspace_id":"%s","title":"Release review"}
                """.formatted(workspaceId)))
        .andExpect(status().isCreated())
        .andExpect(header().string("Location", "/v1/sessions/" + sessionId))
        .andExpect(jsonPath("$.title").value("Release review"));
  }
}
