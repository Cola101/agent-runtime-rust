package com.agentplatform.control.resource;

import java.nio.charset.StandardCharsets;
import java.net.URI;
import java.util.List;
import java.util.Objects;
import java.util.Set;
import java.util.UUID;

public final class RuntimeResourceService {
  /**
   * Scopes an AgentVersion may delegate. This must stay in step with the Tools
   * the Worker actually installs: a scope missing here makes an installed Tool
   * unreachable, and a scope here with no Tool behind it delegates nothing.
   */
  private static final Set<String> BETA_DELEGATED_SCOPES =
      Set.of("tool:workspace.read", "tool:workspace.write", "tool:shell.exec");
  private static final Set<String> PROVIDER_PROTOCOLS = Set.of(
      "openai_compatible", "openai_responses", "anthropic_messages");
  private static final Set<String> SKILL_PLATFORMS = Set.of(
      "darwin-arm64", "linux-arm64", "linux-x86_64");
  private final RuntimeResourceRepository repository;
  private final ProviderCredentialSealer credentialSealer;
  private final SkillArtifactSigner skillArtifactSigner;

  public RuntimeResourceService(RuntimeResourceRepository repository) {
    this(repository, (tenant, provider, credential) -> {
      throw new IllegalStateException("provider credential sealing is not configured");
    }, artifact -> {
      throw new IllegalStateException("skill artifact signing is not configured");
    });
  }

  public RuntimeResourceService(
      RuntimeResourceRepository repository, ProviderCredentialSealer credentialSealer) {
    this(repository, credentialSealer, artifact -> {
      throw new IllegalStateException("skill artifact signing is not configured");
    });
  }

  public RuntimeResourceService(
      RuntimeResourceRepository repository,
      ProviderCredentialSealer credentialSealer,
      SkillArtifactSigner skillArtifactSigner) {
    this.repository = Objects.requireNonNull(repository);
    this.credentialSealer = Objects.requireNonNull(credentialSealer);
    this.skillArtifactSigner = Objects.requireNonNull(skillArtifactSigner);
  }

  public ResourceContext context(UUID tenantId, UUID applicationId) {
    return repository.findContext(required(tenantId), required(applicationId));
  }

  public WorkspaceResource createWorkspace(
      UUID tenantId, UUID applicationId, UUID projectId, String name) {
    return repository.createWorkspace(
        required(tenantId), required(applicationId), required(projectId), text(name, "name", 200));
  }

  public AgentResource createAgent(
      UUID tenantId, UUID applicationId, UUID workspaceId, String name) {
    return repository.createAgent(
        required(tenantId), required(applicationId), required(workspaceId), text(name, "name", 200));
  }

  public AgentVersionResource createAgentVersion(
      UUID tenantId,
      UUID applicationId,
      UUID agentId,
      String instructions,
      List<String> delegatedScopes) {
    return createAgentVersion(
        tenantId, applicationId, agentId, instructions, delegatedScopes, List.of(), List.of());
  }

  public AgentVersionResource createAgentVersion(
      UUID tenantId,
      UUID applicationId,
      UUID agentId,
      String instructions,
      List<String> delegatedScopes,
      List<UUID> skillVersionIds) {
    return createAgentVersion(
        tenantId, applicationId, agentId, instructions, delegatedScopes, skillVersionIds,
        List.of());
  }

  public AgentVersionResource createAgentVersion(
      UUID tenantId,
      UUID applicationId,
      UUID agentId,
      String instructions,
      List<String> delegatedScopes,
      List<UUID> skillVersionIds,
      List<SubagentRoleDefinition> subagentRoles) {
    Objects.requireNonNull(delegatedScopes, "delegatedScopes");
    Objects.requireNonNull(skillVersionIds, "skillVersionIds");
    Objects.requireNonNull(subagentRoles, "subagentRoles");
    if (delegatedScopes.size() > 32) {
      throw new IllegalArgumentException("delegated scopes must contain at most 32 entries");
    }
    var normalizedScopes = delegatedScopes.stream()
        .map(scope -> text(scope, "delegated scope", 200))
        .distinct()
        .sorted()
        .toList();
    if (normalizedScopes.size() != delegatedScopes.size()
        || !BETA_DELEGATED_SCOPES.containsAll(normalizedScopes)) {
      throw new IllegalArgumentException("delegated scopes contain duplicates or unsupported entries");
    }
    var normalizedSkills = skillVersionIds.stream().map(this::required).distinct().toList();
    if (normalizedSkills.size() != skillVersionIds.size() || normalizedSkills.size() > 16) {
      throw new IllegalArgumentException("skill version ids contain duplicates or exceed 16 entries");
    }
    if (subagentRoles.size() > 16) {
      throw new IllegalArgumentException("subagent roles must contain at most 16 entries");
    }
    var normalizedRoles = subagentRoles.stream().map(role -> {
      Objects.requireNonNull(role, "subagent role");
      var name = identifier(role.name(), "subagent role name", 80);
      if ("primary".equals(name)) {
        throw new IllegalArgumentException("primary is reserved for root runs");
      }
      var scopes = role.delegatedScopes().stream()
          .map(scope -> text(scope, "subagent delegated scope", 200))
          .distinct().sorted().toList();
      if (scopes.size() != role.delegatedScopes().size()
          || !normalizedScopes.containsAll(scopes)) {
        throw new IllegalArgumentException(
            "subagent scopes must be unique and contained by the parent AgentVersion");
      }
      return new SubagentRoleDefinition(name, instructionText(role.instructions()), scopes);
    }).toList();
    if (normalizedRoles.stream().map(SubagentRoleDefinition::name).distinct().count()
        != normalizedRoles.size()) {
      throw new IllegalArgumentException("subagent role names must be unique");
    }
    return repository.createAgentVersion(
        required(tenantId), required(applicationId), required(agentId),
        instructionText(instructions), normalizedScopes, normalizedSkills, normalizedRoles);
  }

  public SkillVersionResource publishSkillVersion(
      UUID tenantId,
      UUID applicationId,
      String name,
      String semanticVersion,
      String description,
      String instructions,
      List<String> toolNames,
      List<String> supportedPlatforms,
      String minRuntimeVersion) {
    Objects.requireNonNull(toolNames, "toolNames");
    Objects.requireNonNull(supportedPlatforms, "supportedPlatforms");
    var normalizedTenant = required(tenantId);
    var normalizedApplication = required(applicationId);
    var normalizedName = identifier(name, "skill name", 120);
    var normalizedVersion = semanticVersion(semanticVersion, "semantic version");
    var normalizedTools = toolNames.stream()
        .map(tool -> identifier(tool, "tool name", 120)).distinct().sorted().toList();
    if (normalizedTools.size() != toolNames.size() || normalizedTools.size() > 32) {
      throw new IllegalArgumentException("tool names contain duplicates or exceed 32 entries");
    }
    var normalizedPlatforms = supportedPlatforms.stream()
        .map(platform -> text(platform, "supported platform", 40)).distinct().sorted().toList();
    if (normalizedPlatforms.isEmpty()
        || normalizedPlatforms.size() != supportedPlatforms.size()
        || !SKILL_PLATFORMS.containsAll(normalizedPlatforms)) {
      throw new IllegalArgumentException("supported platforms contain duplicates or unsupported entries");
    }
    var versionId = UUID.randomUUID();
    var artifact = new SkillArtifact(
        1, normalizedTenant, normalizedApplication, versionId, normalizedName,
        normalizedVersion, text(description, "description", 500),
        instructionText(instructions), normalizedTools, normalizedPlatforms,
        semanticVersion(minRuntimeVersion, "minimum runtime version"));
    return repository.publishSkillVersion(artifact, skillArtifactSigner.sign(artifact));
  }

  public ModelPolicyResource createModelPolicy(
      UUID tenantId, UUID applicationId, UUID workspaceId, String name, String routing) {
    return createModelPolicy(
        tenantId, applicationId, workspaceId, name, routing, List.of());
  }

  public ModelProviderResource createModelProvider(
      UUID tenantId,
      UUID applicationId,
      String name,
      String protocol,
      String endpoint,
      String model,
      String apiKey) {
    var normalizedTenant = required(tenantId);
    var normalizedApplication = required(applicationId);
    var normalizedProtocol = text(protocol, "protocol", 40);
    if (!PROVIDER_PROTOCOLS.contains(normalizedProtocol)) {
      throw new IllegalArgumentException("provider protocol is not supported");
    }
    var normalizedEndpoint = providerEndpoint(endpoint);
    var normalizedKey = text(apiKey, "api key", 8_192);
    var providerId = UUID.randomUUID();
    var envelope = credentialSealer.seal(normalizedTenant, providerId, normalizedKey);
    return repository.createModelProvider(
        normalizedTenant, normalizedApplication, providerId, text(name, "name", 200),
        normalizedProtocol, normalizedEndpoint, text(model, "model", 200), envelope);
  }

  public ModelPolicyResource createModelPolicy(
      UUID tenantId,
      UUID applicationId,
      UUID workspaceId,
      String name,
      String routing,
      List<UUID> providerIds) {
    Objects.requireNonNull(providerIds, "providerIds");
    var normalizedProviders = providerIds.stream().map(this::required).distinct().toList();
    if (normalizedProviders.size() != providerIds.size() || normalizedProviders.size() > 8) {
      throw new IllegalArgumentException("provider ids contain duplicates or exceed 8 candidates");
    }
    if ("ordered_failover".equals(routing) && normalizedProviders.isEmpty()) {
      throw new IllegalArgumentException("ordered_failover requires at least one provider");
    }
    if (!Set.of("single_provider", "ordered_failover").contains(routing)) {
      throw new IllegalArgumentException("routing must be single_provider or ordered_failover");
    }
    if ("single_provider".equals(routing) && normalizedProviders.size() > 1) {
      throw new IllegalArgumentException("single_provider accepts at most one provider");
    }
    return repository.createModelPolicy(
        required(tenantId), required(applicationId), required(workspaceId),
        text(name, "name", 200), routing, normalizedProviders);
  }

  public SessionResource createSession(
      UUID tenantId, UUID applicationId, UUID workspaceId, String title) {
    var normalizedTitle = title == null ? null : text(title, "title", 200);
    return repository.createSession(
        required(tenantId), required(applicationId), required(workspaceId), normalizedTitle);
  }

  private UUID required(UUID value) {
    return Objects.requireNonNull(value, "resource identity");
  }

  private String text(String value, String field, int maximum) {
    if (value == null || value.isBlank()) {
      throw new IllegalArgumentException(field + " is required");
    }
    var normalized = value.trim();
    if (normalized.length() > maximum) {
      throw new IllegalArgumentException(field + " exceeds " + maximum + " characters");
    }
    return normalized;
  }

  private String instructionText(String value) {
    var normalized = text(value, "instructions", 32_000);
    if (normalized.getBytes(StandardCharsets.UTF_8).length > 32_000) {
      throw new IllegalArgumentException("instructions exceed 32000 UTF-8 bytes");
    }
    return normalized;
  }

  private String identifier(String value, String field, int maximum) {
    var normalized = text(value, field, maximum);
    if (!normalized.matches("[a-z0-9](?:[a-z0-9._-]*[a-z0-9])?")) {
      throw new IllegalArgumentException(field + " must be a lowercase portable identifier");
    }
    return normalized;
  }

  private String semanticVersion(String value, String field) {
    var normalized = text(value, field, 64);
    if (!normalized.matches(
        "(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?")) {
      throw new IllegalArgumentException(field + " must be a semantic version");
    }
    return normalized;
  }

  private String providerEndpoint(String value) {
    var normalized = text(value, "endpoint", 2_048);
    try {
      var uri = URI.create(normalized);
      var host = uri.getHost();
      var loopback = "localhost".equalsIgnoreCase(host)
          || "127.0.0.1".equals(host)
          || "::1".equals(host);
      if (host == null || uri.getUserInfo() != null || uri.getFragment() != null
          || !("https".equalsIgnoreCase(uri.getScheme())
              || ("http".equalsIgnoreCase(uri.getScheme()) && loopback))) {
        throw new IllegalArgumentException("provider endpoint must use HTTPS or loopback HTTP");
      }
      return uri.toASCIIString();
    } catch (IllegalArgumentException invalid) {
      throw new IllegalArgumentException(
          "provider endpoint must be an absolute HTTPS or loopback HTTP URL", invalid);
    }
  }
}
