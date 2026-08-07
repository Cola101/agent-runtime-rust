#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "socket"

MAX_REQUEST_BYTES = 1_048_576
TOOL_NAME = "workspace.read_text"
TOOL_CALL_ID = "call_native_read_1"
FIXTURE_PATH = "README.txt"
FIXTURE_TEXT = "Agent Runtime native workspace: trusted read-only Tool fixture."

def required_environment(name)
  value = ENV.fetch(name, "").strip
  raise "#{name} is required" if value.empty?

  value
end

def parse_port
  port = Integer(required_environment("AGENT_RUNTIME_TEST_PROVIDER_PORT"), 10)
  raise "provider port is outside 1..65535" unless (1..65_535).cover?(port)

  port
rescue ArgumentError
  raise "AGENT_RUNTIME_TEST_PROVIDER_PORT must be an integer"
end

def read_request(socket)
  request_line = socket.gets("\n", 8_193)
  raise "request line is missing or too long" if request_line.nil? || request_line.bytesize > 8_192

  method, target, version = request_line.strip.split(" ", 3)
  raise "only POST /v1/chat/completions is accepted" unless method == "POST" &&
                                                              target == "/v1/chat/completions" &&
                                                              version&.start_with?("HTTP/1.")

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

def validate_common!(headers, body, expected_bearer)
  raise "provider authorization failed" unless headers["authorization"] == "Bearer #{expected_bearer}"
  raise "provider request must be streamed" unless body["stream"] == true
  raise "provider request messages are missing" unless body["messages"].is_a?(Array)
end

def validate_first_turn!(body, expected_system_instructions)
  tool_names = Array(body["tools"]).map { |tool| tool.dig("function", "name") }.compact
  raise "trusted workspace Tool was not advertised" unless tool_names.include?(TOOL_NAME)
  system_messages = body["messages"].select { |message| message["role"] == "system" }
  unless system_messages == [{ "role" => "system", "content" => expected_system_instructions }]
    raise "immutable Agent instructions were not preserved as one exact system message"
  end
  raise "first model turn already contains a Tool result" if body["messages"].any? do |message|
    message["role"] == "tool"
  end
end

def validate_second_turn!(body)
  assistant = body["messages"].find do |message|
    Array(message["tool_calls"]).any? { |call| call["id"] == TOOL_CALL_ID }
  end
  call = Array(assistant&.fetch("tool_calls", nil)).find { |item| item["id"] == TOOL_CALL_ID }
  raise "assistant Tool call history is missing" unless call&.dig("function", "name") == TOOL_NAME
  arguments = JSON.parse(call.dig("function", "arguments"))
  raise "assistant Tool call path changed" unless arguments == { "path" => FIXTURE_PATH }

  tool_result = body["messages"].find do |message|
    message["role"] == "tool" && message["tool_call_id"] == TOOL_CALL_ID
  end
  raise "bound Tool result history is missing" unless tool_result

  content = JSON.parse(tool_result.fetch("content"))
  raise "Tool result path changed" unless content["path"] == FIXTURE_PATH
  raise "Tool result content was not preserved" unless content["text"]&.strip == FIXTURE_TEXT
end

def sse_response(body)
  [
    "HTTP/1.1 200 OK",
    "Content-Type: text/event-stream; charset=utf-8",
    "Cache-Control: no-cache",
    "Connection: close",
    "Content-Length: #{body.bytesize}",
    "",
    body
  ].join("\r\n")
end

def first_turn_response
  chunk = {
    "choices" => [{
      "index" => 0,
      "delta" => {
        "tool_calls" => [{
          "index" => 0,
          "id" => TOOL_CALL_ID,
          "function" => {
            "name" => TOOL_NAME,
            "arguments" => JSON.generate("path" => FIXTURE_PATH)
          }
        }]
      },
      "finish_reason" => "tool_calls"
    }],
    "usage" => { "prompt_tokens" => 20, "completion_tokens" => 8 }
  }
  "data: #{JSON.generate(chunk)}\n\ndata: [DONE]\n\n"
end

def second_turn_response
  chunk = {
    "choices" => [{
      "index" => 0,
      "delta" => { "content" => "trusted workspace content verified" },
      "finish_reason" => "stop"
    }],
    "usage" => { "prompt_tokens" => 40, "completion_tokens" => 5 }
  }
  "data: #{JSON.generate(chunk)}\n\ndata: [DONE]\n\n"
end

port = parse_port
expected_bearer = required_environment("AGENT_RUNTIME_TEST_PROVIDER_API_KEY")
expected_system_instructions = required_environment(
  "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_SYSTEM_INSTRUCTIONS"
)
ready_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_READY_FILE")
evidence_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE")
server = TCPServer.new("127.0.0.1", port)
File.write(ready_file, "#{port}\n", mode: "w", perm: 0o600)

begin
  [first_turn_response, second_turn_response].each_with_index do |response_body, index|
    socket = server.accept
    begin
      headers, body = read_request(socket)
      validate_common!(headers, body, expected_bearer)
      index.zero? ? validate_first_turn!(body, expected_system_instructions) : validate_second_turn!(body)
      socket.write(sse_response(response_body))
    ensure
      socket.close
    end
  end
  evidence = {
    "requests" => 2,
    "tool" => TOOL_NAME,
    "path" => FIXTURE_PATH,
    "result_verified" => true,
    "system_instructions_verified" => true
  }
  File.write(evidence_file, JSON.pretty_generate(evidence), mode: "w", perm: 0o600)
ensure
  server.close
  File.unlink(ready_file) if File.exist?(ready_file)
end
