#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "net/http"
require "open3"
require "socket"
require "tmpdir"
require "uri"

ROOT = File.expand_path("../..", __dir__)
PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "rate_limited_provider.rb")

def free_loopback_port
  server = TCPServer.new("127.0.0.1", 0)
  server.addr[1]
ensure
  server&.close
end

Dir.mktmpdir("agent-runtime-rate-limit-provider-") do |temporary|
  port = free_loopback_port
  ready = File.join(temporary, "ready")
  evidence = File.join(temporary, "evidence.json")
  secret = "rate-limit-provider-secret"
  environment = {
    "AGENT_RUNTIME_TEST_PROVIDER_PORT" => port.to_s,
    "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => secret,
    "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => ready,
    "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => evidence,
    "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_MODEL" => "rate-limited-model",
    "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_REQUESTS" => "2"
  }

  Open3.popen3(environment, PROVIDER, chdir: ROOT) do |stdin, stdout, stderr, child|
    stdin.close
    100.times do
      break if File.file?(ready)
      raise "provider exited before becoming ready" unless child.alive?

      sleep 0.05
    end
    raise "provider did not become ready" unless File.file?(ready)

    uri = URI("http://127.0.0.1:#{port}/v1/chat/completions")
    2.times do
      request = Net::HTTP::Post.new(uri)
      request["Authorization"] = "Bearer #{secret}"
      request["Content-Type"] = "application/json"
      request.body = JSON.generate(
        "model" => "rate-limited-model",
        "stream" => true,
        "messages" => [{ "role" => "user", "content" => "test safe fallback" }]
      )
      response = Net::HTTP.start(uri.host, uri.port, nil) { |http| http.request(request) }
      raise "expected HTTP 429, got #{response.code}" unless response.code == "429"
    end

    status = child.value
    output = stdout.read
    error = stderr.read
    raise "rate-limited provider failed: #{error}" unless status.success?
    raise "provider emitted unexpected output" unless output.empty? && error.empty?
    raise "provider leaked its bearer token" if (output + error).include?(secret)
  end

  result = JSON.parse(File.read(evidence))
  raise "rate-limit evidence is incomplete: #{result}" unless result == {
    "requests" => 2,
    "authorization_verified" => true,
    "model_verified" => true,
    "status" => 429
  }
end

puts "validated deterministic rate-limited loopback Provider"
