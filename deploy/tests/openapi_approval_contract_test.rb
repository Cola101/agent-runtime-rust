#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

ROOT = File.expand_path("../..", __dir__)
contract = YAML.safe_load(
  File.read(File.join(ROOT, "contracts", "openapi", "openapi.yaml")),
  permitted_classes: [],
  aliases: false
)

list_operation = contract.dig("paths", "/v1/approvals", "get")
raise "OpenAPI omits the pending approval list" unless list_operation

parameters = list_operation.fetch("parameters").to_h do |parameter|
  [parameter.fetch("name"), parameter]
end
status_schema = parameters.fetch("status").fetch("schema")
limit_schema = parameters.fetch("limit").fetch("schema")
unless status_schema == { "type" => "string", "enum" => ["pending"], "default" => "pending" }
  raise "approval status query contract must only accept pending"
end
unless limit_schema.values_at("type", "minimum", "maximum", "default") == ["integer", 1, 100, 50]
  raise "approval page limit contract must describe the 1..100 boundary"
end

responses = list_operation.fetch("responses")
unless responses.dig("200", "content", "application/json", "schema", "$ref") ==
       "#/components/schemas/ApprovalListResponse"
  raise "approval list success schema is not bound"
end
%w[400 401 403].each do |status|
  unless responses.dig(status, "$ref") ||
         responses.dig(status, "content", "application/problem+json", "schema", "$ref")
    raise "approval list omits HTTP #{status} contract"
  end
end

decision_operation = contract.dig("paths", "/v1/approvals/{approvalId}:decide", "post")
raise "OpenAPI omits the approval decision" unless decision_operation

decision_schema = contract.dig("components", "schemas", "ApprovalDecisionRequest")
unless decision_schema.fetch("required").sort == %w[decision version] &&
       decision_schema.dig("properties", "decision", "enum") == %w[allow_once allow_session deny] &&
       decision_schema.dig("properties", "version", "minimum") == 1
  raise "approval decision request does not preserve versioned allow-once/deny semantics"
end
%w[400 401 403 404 409].each do |status|
  responses = decision_operation.fetch("responses")
  unless responses.dig(status, "$ref") ||
         responses.dig(status, "content", "application/problem+json", "schema", "$ref")
    raise "approval decision omits HTTP #{status} contract"
  end
end

summary = contract.dig("components", "schemas", "ApprovalSummary")
review_binding = %w[
  tool_name tool_call_id effect sandbox binding_digest arguments policy_digest
  session_scope_digest policy_snapshot available_decisions
]
unless (review_binding - summary.fetch("required")).empty?
  raise "approval summary does not require the immutable Tool review binding"
end
unless summary.dig("properties", "available_decisions", "items", "enum") ==
       %w[allow_once allow_session deny] &&
       contract.dig("components", "schemas", "ApprovalPolicySnapshot", "required").sort ==
       %w[approval effect implementation_digest required_scopes sandbox tool_name]
  raise "approval summary does not describe policy-bound session grants"
end

puts "validated approval OpenAPI success, concurrency, and policy-bound session grant contracts"
