#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "socket"

MAX_REQUEST_BYTES = 1_048_576

def required_environment(name)
  value = ENV.fetch(name, "").strip
  raise "#{name} is required" if value.empty?

  value
end

def positive_integer_environment(name)
  value = Integer(required_environment(name), 10)
  raise "#{name} must be positive" unless value.positive?

  value
rescue ArgumentError
  raise "#{name} must be an integer"
end

def read_request(socket)
  request_line = socket.gets("\n", 8_193)
  raise "request line is missing or too long" if request_line.nil? || request_line.bytesize > 8_192

  method, target, version = request_line.strip.split(" ", 3)
  unless method == "POST" && target == "/v1/chat/completions" && version&.start_with?("HTTP/1.")
    raise "only POST /v1/chat/completions is accepted"
  end

  headers = {}
  loop do
    line = socket.gets("\n", 8_193)
    raise "request headers ended unexpectedly" if line.nil?
    raise "request header is too long" if line.bytesize > 8_192

    line = line.chomp.delete_suffix("\r")
    break if line.empty?

    name, value = line.split(":", 2)
    raise "malformed request header" if value.nil?

    headers[name.downcase] = value.strip
  end
  length = Integer(headers.fetch("content-length", ""), 10)
  raise "request body is outside the supported size" unless (1..MAX_REQUEST_BYTES).cover?(length)

  body = socket.read(length)
  raise "request body ended unexpectedly" unless body&.bytesize == length

  [headers, JSON.parse(body)]
rescue JSON::ParserError, ArgumentError
  raise "request body is not valid bounded JSON"
end

def rate_limited_response
  body = JSON.generate("error" => { "type" => "rate_limit", "message" => "try another route" })
  [
    "HTTP/1.1 429 Too Many Requests",
    "Content-Type: application/json",
    "Retry-After: 1",
    "Connection: close",
    "Content-Length: #{body.bytesize}",
    "",
    body
  ].join("\r\n")
end

port = positive_integer_environment("AGENT_RUNTIME_TEST_PROVIDER_PORT")
raise "provider port is outside 1..65535" unless (1..65_535).cover?(port)

expected_bearer = required_environment("AGENT_RUNTIME_TEST_PROVIDER_API_KEY")
expected_model = required_environment("AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_MODEL")
expected_requests = positive_integer_environment("AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_REQUESTS")
ready_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_READY_FILE")
evidence_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE")
server = TCPServer.new("127.0.0.1", port)
File.write(ready_file, "#{port}\n", mode: "w", perm: 0o600)

begin
  expected_requests.times do
    socket = server.accept
    begin
      headers, body = read_request(socket)
      raise "provider authorization failed" unless headers["authorization"] == "Bearer #{expected_bearer}"
      raise "provider request must be streamed" unless body["stream"] == true
      raise "provider model changed" unless body["model"] == expected_model

      socket.write(rate_limited_response)
    ensure
      socket.close
    end
  end
  evidence = {
    "requests" => expected_requests,
    "authorization_verified" => true,
    "model_verified" => true,
    "status" => 429
  }
  File.write(evidence_file, JSON.pretty_generate(evidence), mode: "w", perm: 0o600)
ensure
  server.close
  File.unlink(ready_file) if File.exist?(ready_file)
end
