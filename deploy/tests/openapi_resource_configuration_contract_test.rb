#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

ROOT = File.expand_path("../..", __dir__)
contract = YAML.safe_load(
  File.read(File.join(ROOT, "contracts", "openapi", "openapi.yaml")),
  permitted_classes: [],
  aliases: false
)

paths = contract.fetch("paths")
expected_operations = {
  ["/v1/console/resource-context", "get"] => "getConsoleResourceContext",
  ["/v1/workspaces", "post"] => "createWorkspace",
  ["/v1/agents", "post"] => "createAgent",
  ["/v1/agents/{agentId}/versions", "post"] => "createAgentVersion",
  ["/v1/skills:publish", "post"] => "publishSkillVersion",
  ["/v1/model-providers", "post"] => "createModelProvider",
  ["/v1/model-policies", "post"] => "createModelPolicy",
  ["/v1/sessions", "post"] => "createSession"
}
expected_operations.each do |(path, method), operation_id|
  operation = paths.dig(path, method)
  raise "OpenAPI omits #{method.upcase} #{path}" unless operation
  raise "#{path} has the wrong operationId" unless operation["operationId"] == operation_id
end

workspace_request = contract.dig("components", "schemas", "CreateWorkspaceRequest")
unless workspace_request.fetch("required").sort == %w[name project_id] &&
       !workspace_request.fetch("properties").key?("application_id") &&
       !workspace_request.fetch("properties").key?("tenant_id")
  raise "Workspace creation must derive tenant and application from authorization claims"
end

version_request = contract.dig("components", "schemas", "CreateAgentVersionRequest")
version_response = contract.dig("components", "schemas", "AgentVersion")
subagent_role = contract.dig("components", "schemas", "SubagentRole")
unless version_request.fetch("required").sort == %w[delegated_scopes instructions] &&
       version_request.dig("properties", "instructions", "maxLength") == 32_000 &&
       version_request.dig("properties", "delegated_scopes", "maxItems") == 32 &&
       version_request.dig("properties", "skill_version_ids", "maxItems") == 16 &&
       version_request.dig("properties", "skill_version_ids", "uniqueItems") == true &&
       version_request.dig("properties", "subagent_roles", "maxItems") == 16 &&
       version_response.fetch("required").include?("subagent_roles") &&
       subagent_role.fetch("required").sort == %w[delegated_scopes instructions name] &&
       subagent_role.dig("properties", "name", "maxLength") == 80 &&
       subagent_role.dig("properties", "instructions", "maxLength") == 32_000
  raise "AgentVersion must bind bounded instructions, scopes, Skills, and subagent roles"
end

skill_request = contract.dig("components", "schemas", "PublishSkillVersionRequest")
skill_response = contract.dig("components", "schemas", "SkillVersion")
unless skill_request.fetch("required").sort ==
       %w[description instructions min_runtime_version name semantic_version supported_platforms tool_names] &&
       skill_request.dig("properties", "tool_names", "maxItems") == 32 &&
       skill_request.dig("properties", "supported_platforms", "uniqueItems") == true &&
       skill_response.fetch("required").include?("artifact_digest") &&
       skill_response.fetch("required").include?("signature")
  raise "SkillVersion must expose an immutable signed and platform-bounded artifact"
end

model_policy_request = contract.dig("components", "schemas", "CreateModelPolicyRequest")
unless model_policy_request.fetch("required").sort == %w[name routing workspace_id] &&
       model_policy_request.dig("properties", "routing", "enum") ==
       %w[single_provider ordered_failover] &&
       model_policy_request.dig("properties", "provider_ids", "maxItems") == 8 &&
       model_policy_request.dig("properties", "provider_ids", "uniqueItems") == true
  raise "ModelPolicy must expose the bounded immutable Provider candidate chain"
end

provider_request = contract.dig("components", "schemas", "CreateModelProviderRequest")
provider_response = contract.dig("components", "schemas", "ModelProvider")
unless provider_request.fetch("required").sort == %w[api_key endpoint model name protocol] &&
       provider_request.dig("properties", "api_key", "writeOnly") == true &&
       provider_request.dig("properties", "protocol", "enum") ==
       %w[openai_compatible openai_responses anthropic_messages] &&
       !provider_response.fetch("properties").key?("api_key") &&
       !provider_response.fetch("properties").key?("credential_envelope")
  raise "Provider BYOK input must be write-only and absent from every response schema"
end

context = contract.dig("components", "schemas", "ConsoleResourceContext")
unless context.fetch("required").sort == %w[application_id application_name projects] &&
       context.dig("properties", "projects", "items", "$ref") ==
       "#/components/schemas/ProjectSummary"
  raise "Console resource context must identify the authorized Application and Projects"
end

%w[Workspace Agent AgentVersion SkillVersion ModelProvider ModelPolicy Session].each do |schema_name|
  schema = contract.dig("components", "schemas", schema_name)
  raise "OpenAPI omits #{schema_name}" unless schema
  if schema.fetch("properties").key?("tenant_id") || schema.fetch("properties").key?("application_id")
    raise "#{schema_name} must not echo authorization boundary identifiers"
  end
end

puts "validated application-scoped runtime configuration OpenAPI contract"
