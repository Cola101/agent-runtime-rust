package com.agentplatform.control.api;

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
import com.agentplatform.control.security.TenantContext;
import com.fasterxml.jackson.annotation.JsonProperty;
import jakarta.validation.Valid;
import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.NotNull;
import jakarta.validation.constraints.Pattern;
import jakarta.validation.constraints.Size;
import java.net.URI;
import java.util.List;
import java.util.UUID;
import org.springframework.http.ResponseEntity;
import org.springframework.security.core.annotation.AuthenticationPrincipal;
import org.springframework.security.oauth2.jwt.Jwt;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

@RestController
@RequestMapping("/v1")
public class RuntimeResourceController {
  private final RuntimeResourceService resources;

  public RuntimeResourceController(RuntimeResourceService resources) {
    this.resources = resources;
  }

  @GetMapping("/console/resource-context")
  ResourceContextResponse resourceContext(@AuthenticationPrincipal Jwt jwt) {
    var context = TenantContext.from(jwt);
    return ResourceContextResponse.from(resources.context(context.tenantId(), context.applicationId()));
  }

  @PostMapping("/workspaces")
  ResponseEntity<WorkspaceResponse> createWorkspace(
      @AuthenticationPrincipal Jwt jwt, @Valid @RequestBody CreateWorkspaceRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.createWorkspace(
        context.tenantId(), context.applicationId(), request.projectId(), request.name());
    return created("/v1/workspaces/" + created.id(), WorkspaceResponse.from(created));
  }

  @PostMapping("/agents")
  ResponseEntity<AgentResponse> createAgent(
      @AuthenticationPrincipal Jwt jwt, @Valid @RequestBody CreateAgentRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.createAgent(
        context.tenantId(), context.applicationId(), request.workspaceId(), request.name());
    return created("/v1/agents/" + created.id(), AgentResponse.from(created));
  }

  @PostMapping("/agents/{agentId}/versions")
  ResponseEntity<AgentVersionResponse> createAgentVersion(
      @AuthenticationPrincipal Jwt jwt,
      @PathVariable UUID agentId,
      @Valid @RequestBody CreateAgentVersionRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.createAgentVersion(
        context.tenantId(), context.applicationId(), agentId,
        request.instructions(), request.delegatedScopes(), request.skillVersionIds(),
        request.subagentRoles().stream().map(SubagentRoleRequest::toDefinition).toList(),
        request.toolApprovalPolicies());
    return created(
        "/v1/agents/" + agentId + "/versions/" + created.id(),
        AgentVersionResponse.from(created));
  }

  @PostMapping("/skills:publish")
  ResponseEntity<SkillVersionResponse> publishSkillVersion(
      @AuthenticationPrincipal Jwt jwt,
      @Valid @RequestBody PublishSkillVersionRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.publishSkillVersion(
        context.tenantId(), context.applicationId(), request.name(), request.semanticVersion(),
        request.description(), request.instructions(), request.toolNames(),
        request.supportedPlatforms(), request.minRuntimeVersion());
    return created(
        "/v1/skill-versions/" + created.id(), SkillVersionResponse.from(created));
  }

  @PostMapping("/model-policies")
  ResponseEntity<ModelPolicyResponse> createModelPolicy(
      @AuthenticationPrincipal Jwt jwt, @Valid @RequestBody CreateModelPolicyRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.createModelPolicy(
        context.tenantId(), context.applicationId(), request.workspaceId(),
        request.name(), request.routing(),
        request.providerIds() == null ? List.of() : request.providerIds());
    return created("/v1/model-policies/" + created.id(), ModelPolicyResponse.from(created));
  }

  @PostMapping("/model-providers")
  ResponseEntity<ModelProviderResponse> createModelProvider(
      @AuthenticationPrincipal Jwt jwt, @Valid @RequestBody CreateModelProviderRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.createModelProvider(
        context.tenantId(), context.applicationId(), request.name(), request.protocol(),
        request.endpoint(), request.model(), request.apiKey());
    return created("/v1/model-providers/" + created.id(), ModelProviderResponse.from(created));
  }

  @PostMapping("/sessions")
  ResponseEntity<SessionResponse> createSession(
      @AuthenticationPrincipal Jwt jwt, @Valid @RequestBody CreateSessionRequest request) {
    var context = TenantContext.from(jwt);
    var created = resources.createSession(
        context.tenantId(), context.applicationId(), request.workspaceId(), request.title());
    return created("/v1/sessions/" + created.id(), SessionResponse.from(created));
  }

  private <T> ResponseEntity<T> created(String location, T body) {
    return ResponseEntity.created(URI.create(location)).body(body);
  }

  record CreateWorkspaceRequest(
      @NotNull @JsonProperty("project_id") UUID projectId,
      @NotBlank @Size(max = 200) String name) {}

  record CreateAgentRequest(
      @NotNull @JsonProperty("workspace_id") UUID workspaceId,
      @NotBlank @Size(max = 200) String name) {}

  record CreateAgentVersionRequest(
      @NotBlank @Size(max = 32_000) String instructions,
      @NotNull @Size(max = 32) @JsonProperty("delegated_scopes")
      List<@NotBlank @Size(max = 200) String> delegatedScopes,
      @Size(max = 16) @JsonProperty("skill_version_ids") List<@NotNull UUID> skillVersionIds,
      @Valid @Size(max = 16) @JsonProperty("subagent_roles")
      List<@NotNull SubagentRoleRequest> subagentRoles,
      /**
       * Per-Tool approval policy. Absent means every Tool asks, which is what a
       * client that says nothing should get.
       */
      @Size(max = 16) @JsonProperty("tool_approval_policies")
      java.util.Map<@NotBlank String, @NotBlank String> toolApprovalPolicies) {
    CreateAgentVersionRequest {
      skillVersionIds = skillVersionIds == null ? List.of() : List.copyOf(skillVersionIds);
      subagentRoles = subagentRoles == null ? List.of() : List.copyOf(subagentRoles);
      // Absent means every Tool asks. A client that says nothing about policy
      // must not be read as having asked for one.
      toolApprovalPolicies = toolApprovalPolicies == null
          ? java.util.Map.of()
          : java.util.Map.copyOf(toolApprovalPolicies);
    }
  }

  record SubagentRoleRequest(
      @NotBlank @Size(max = 80) String name,
      @NotBlank @Size(max = 32_000) String instructions,
      @NotNull @Size(max = 32) @JsonProperty("delegated_scopes")
      List<@NotBlank @Size(max = 200) String> delegatedScopes) {
    SubagentRoleDefinition toDefinition() {
      return new SubagentRoleDefinition(name, instructions, delegatedScopes);
    }
  }

  record PublishSkillVersionRequest(
      @NotBlank @Size(max = 120) String name,
      @NotBlank @Size(max = 64) @JsonProperty("semantic_version") String semanticVersion,
      @NotBlank @Size(max = 500) String description,
      @NotBlank @Size(max = 32_000) String instructions,
      @NotNull @Size(max = 32) @JsonProperty("tool_names")
      List<@NotBlank @Size(max = 120) String> toolNames,
      @NotNull @Size(min = 1, max = 3) @JsonProperty("supported_platforms")
      List<@NotBlank @Size(max = 40) String> supportedPlatforms,
      @NotBlank @Size(max = 64) @JsonProperty("min_runtime_version")
      String minRuntimeVersion) {}

  record CreateModelPolicyRequest(
      @NotNull @JsonProperty("workspace_id") UUID workspaceId,
      @NotBlank @Size(max = 200) String name,
      @NotBlank String routing,
      @Size(max = 8) @JsonProperty("provider_ids") List<UUID> providerIds) {}

  record CreateModelProviderRequest(
      @NotBlank @Size(max = 200) String name,
      @NotBlank @Pattern(
          regexp = "openai_compatible|openai_responses|anthropic_messages") String protocol,
      @NotBlank @Size(max = 2_048) String endpoint,
      @NotBlank @Size(max = 200) String model,
      @NotBlank @Size(max = 8_192) @JsonProperty("api_key") String apiKey) {}

  record CreateSessionRequest(
      @NotNull @JsonProperty("workspace_id") UUID workspaceId,
      @Size(max = 200) @Pattern(regexp = ".*\\S.*") String title) {}

  record ProjectResponse(UUID id, String name) {
    static ProjectResponse from(ProjectSummary project) {
      return new ProjectResponse(project.id(), project.name());
    }
  }

  record ResourceContextResponse(
      @JsonProperty("application_id") UUID applicationId,
      @JsonProperty("application_name") String applicationName,
      List<ProjectResponse> projects) {
    static ResourceContextResponse from(ResourceContext context) {
      return new ResourceContextResponse(
          context.applicationId(), context.applicationName(),
          context.projects().stream().map(ProjectResponse::from).toList());
    }
  }

  record WorkspaceResponse(
      UUID id,
      @JsonProperty("project_id") UUID projectId,
      String name,
      String state,
      @JsonProperty("created_at") String createdAt) {
    static WorkspaceResponse from(WorkspaceResource workspace) {
      return new WorkspaceResponse(
          workspace.id(), workspace.projectId(), workspace.name(), workspace.state(),
          workspace.createdAt().toString());
    }
  }

  record AgentResponse(
      UUID id,
      @JsonProperty("workspace_id") UUID workspaceId,
      String name,
      @JsonProperty("created_at") String createdAt) {
    static AgentResponse from(AgentResource agent) {
      return new AgentResponse(
          agent.id(), agent.workspaceId(), agent.name(), agent.createdAt().toString());
    }
  }

  record AgentVersionResponse(
      UUID id,
      @JsonProperty("agent_id") UUID agentId,
      int version,
      String instructions,
      @JsonProperty("delegated_scopes") List<String> delegatedScopes,
      @JsonProperty("skill_version_ids") List<UUID> skillVersionIds,
      @JsonProperty("subagent_roles") List<SubagentRoleResponse> subagentRoles,
      @JsonProperty("created_at") String createdAt) {
    static AgentVersionResponse from(AgentVersionResource version) {
      return new AgentVersionResponse(
          version.id(), version.agentId(), version.version(), version.instructions(),
          version.delegatedScopes(), version.skillVersionIds(),
          version.subagentRoles().stream().map(SubagentRoleResponse::from).toList(),
          version.createdAt().toString());
    }
  }

  record SubagentRoleResponse(
      String name,
      String instructions,
      @JsonProperty("delegated_scopes") List<String> delegatedScopes) {
    static SubagentRoleResponse from(SubagentRoleDefinition role) {
      return new SubagentRoleResponse(role.name(), role.instructions(), role.delegatedScopes());
    }
  }

  record SkillVersionResponse(
      UUID id,
      @JsonProperty("skill_id") UUID skillId,
      String name,
      @JsonProperty("semantic_version") String semanticVersion,
      String description,
      String instructions,
      @JsonProperty("tool_names") List<String> toolNames,
      @JsonProperty("supported_platforms") List<String> supportedPlatforms,
      @JsonProperty("min_runtime_version") String minRuntimeVersion,
      @JsonProperty("artifact_digest") String artifactDigest,
      @JsonProperty("signing_key_id") String signingKeyId,
      String signature,
      @JsonProperty("created_at") String createdAt) {
    static SkillVersionResponse from(SkillVersionResource skill) {
      return new SkillVersionResponse(
          skill.id(), skill.skillId(), skill.name(), skill.semanticVersion(), skill.description(),
          skill.instructions(), skill.toolNames(), skill.supportedPlatforms(),
          skill.minRuntimeVersion(), skill.artifactDigest(), skill.signingKeyId(),
          skill.signature(), skill.createdAt().toString());
    }
  }

  record ModelPolicyResponse(
      UUID id,
      @JsonProperty("workspace_id") UUID workspaceId,
      String name,
      String routing,
      @JsonProperty("provider_ids") List<UUID> providerIds,
      @JsonProperty("created_at") String createdAt) {
    static ModelPolicyResponse from(ModelPolicyResource policy) {
      return new ModelPolicyResponse(
          policy.id(), policy.workspaceId(), policy.name(), policy.routing(),
          policy.providerIds(),
          policy.createdAt().toString());
    }
  }

  record ModelProviderResponse(
      UUID id,
      String name,
      String protocol,
      String endpoint,
      String model,
      String state,
      @JsonProperty("credential_status") String credentialStatus,
      @JsonProperty("created_at") String createdAt) {
    static ModelProviderResponse from(ModelProviderResource provider) {
      return new ModelProviderResponse(
          provider.id(), provider.name(), provider.protocol(), provider.endpoint(),
          provider.model(), provider.state(), provider.credentialStatus(),
          provider.createdAt().toString());
    }
  }

  record SessionResponse(
      UUID id,
      @JsonProperty("workspace_id") UUID workspaceId,
      String title,
      String state,
      @JsonProperty("created_at") String createdAt) {
    static SessionResponse from(SessionResource session) {
      return new SessionResponse(
          session.id(), session.workspaceId(), session.title(), session.state(),
          session.createdAt().toString());
    }
  }
}
