#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

root = File.expand_path("../..", __dir__)
contract = YAML.safe_load(
  File.read(File.join(root, "contracts", "openapi", "openapi.yaml")),
  permitted_classes: [],
  aliases: false
)

operation = contract.dig("paths", "/v1/runs/{runId}:steer", "post")
raise "OpenAPI omits durable Run steering" unless operation
raise "Run steering has the wrong operationId" unless operation["operationId"] == "steerRun"

parameter_refs = operation.fetch("parameters").map { |parameter| parameter.fetch("$ref") }
unless parameter_refs == [
  "#/components/parameters/RunId",
  "#/components/parameters/IdempotencyKey"
]
  raise "Run steering must be path- and idempotency-bound"
end

request = contract.dig("components", "schemas", "SteerRunRequest")
unless request.fetch("required") == ["input"] &&
       request.dig("properties", "input", "minLength") == 1 &&
       request.dig("properties", "input", "maxLength") == 32_768
  raise "Run steering input must be present and bounded"
end

accepted = contract.dig("components", "schemas", "RunSteeringAccepted")
unless accepted.fetch("required").sort == %w[run_id state steering_id] &&
       accepted.dig("properties", "state", "enum") == %w[pending applied rejected cancelled]
  raise "Run steering response must expose its durable command identity and state"
end

responses = operation.fetch("responses")
unless responses.dig("202", "content", "application/json", "schema", "$ref") ==
       "#/components/schemas/RunSteeringAccepted"
  raise "Run steering acceptance is not bound to its durable response"
end
%w[400 401 403 404 409 429].each do |status|
  unless responses.dig(status, "$ref") ||
         responses.dig(status, "content", "application/problem+json", "schema", "$ref")
    raise "Run steering omits HTTP #{status} contract"
  end
end
unless responses.dig("429", "headers", "Retry-After", "schema", "type") == "integer" &&
       responses.dig("429", "content", "application/problem+json", "schema", "$ref") ==
         "#/components/schemas/Problem"
  raise "Run steering rate limit must expose Retry-After and a problem response"
end

puts "validated idempotent, bounded and durable Run steering OpenAPI contract"
