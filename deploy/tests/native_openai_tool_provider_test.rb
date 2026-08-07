#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "net/http"
require "open3"
require "socket"
require "tmpdir"
require "uri"

ROOT = File.expand_path("../..", __dir__)
PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "openai_tool_provider.rb")

def free_loopback_port
  server = TCPServer.new("127.0.0.1", 0)
  server.addr[1]
ensure
  server&.close
end

def wait_for_file(path, child)
  100.times do
    return if File.file?(path)
    raise "provider exited before becoming ready" unless child.alive?

    sleep 0.05
  end
  raise "provider did not become ready"
end

def post_json(uri, bearer, body)
  request = Net::HTTP::Post.new(uri)
  request["Authorization"] = "Bearer #{bearer}"
  request["Content-Type"] = "application/json"
  request.body = JSON.generate(body)
  Net::HTTP.start(uri.host, uri.port, nil) { |http| http.request(request) }
end

Dir.mktmpdir("agent-runtime-tool-provider-test-") do |temporary|
  port = free_loopback_port
  ready = File.join(temporary, "ready")
  evidence = File.join(temporary, "evidence.json")
  secret = "local-provider-test-secret"
  system_instructions = "Inspect evidence before taking action."
  environment = {
    "AGENT_RUNTIME_TEST_PROVIDER_PORT" => port.to_s,
    "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => secret,
    "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => ready,
    "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => evidence,
    "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_SYSTEM_INSTRUCTIONS" => system_instructions
  }

  Open3.popen3(environment, PROVIDER, chdir: ROOT) do |stdin, stdout, stderr, child|
    stdin.close
    wait_for_file(ready, child)
    uri = URI("http://127.0.0.1:#{port}/v1/chat/completions")

    first = post_json(
      uri,
      secret,
      {
        "model" => "local-tool-test",
        "stream" => true,
        "messages" => [
          { "role" => "system", "content" => system_instructions },
          { "role" => "user", "content" => "read the workspace fixture" }
        ],
        "tools" => [{
          "type" => "function",
          "function" => { "name" => "workspace.read_text", "parameters" => { "type" => "object" } }
        }]
      }
    )
    raise "first provider request failed: #{first.code} #{first.body}" unless first.code == "200"
    raise "provider did not request the trusted Tool" unless first.body.include?("workspace.read_text")
    raise "provider did not finish with tool_calls" unless first.body.include?(%q("finish_reason":"tool_calls"))

    second = post_json(
      uri,
      secret,
      {
        "model" => "local-tool-test",
        "stream" => true,
        "messages" => [
          { "role" => "system", "content" => system_instructions },
          { "role" => "user", "content" => "read the workspace fixture" },
          {
            "role" => "assistant",
            "content" => nil,
            "tool_calls" => [{
              "id" => "call_native_read_1",
              "type" => "function",
              "function" => {
                "name" => "workspace.read_text",
                "arguments" => JSON.generate("path" => "README.txt")
              }
            }]
          },
          {
            "role" => "tool",
            "tool_call_id" => "call_native_read_1",
            "content" => JSON.generate(
              "path" => "README.txt",
              "text" => "Agent Runtime native workspace: trusted read-only Tool fixture."
            )
          }
        ]
      }
    )
    raise "second provider request failed: #{second.code} #{second.body}" unless second.code == "200"
    raise "provider did not return the final answer" unless second.body.include?("trusted workspace content verified")

    child.value
    output = stdout.read
    error = stderr.read
    raise "provider wrote unexpected stdout: #{output}" unless output.empty?
    raise "provider failed: #{error}" unless child.value.success?
    raise "provider leaked its bearer token" if (output + error).include?(secret)
  end

  result = JSON.parse(File.read(evidence))
  raise "provider did not verify both model turns" unless result == {
    "requests" => 2,
    "tool" => "workspace.read_text",
    "path" => "README.txt",
    "result_verified" => true,
    "system_instructions_verified" => true
  }
end

puts "validated deterministic loopback Tool provider"
