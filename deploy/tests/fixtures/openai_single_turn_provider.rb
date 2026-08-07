#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "json"
require "socket"

def required_environment(name)
  value = ENV.fetch(name, "")
  raise "#{name} is required" if value.empty?

  value
end

def positive_port
  port = Integer(required_environment("AGENT_RUNTIME_TEST_PROVIDER_PORT"), 10)
  raise "Provider port is outside 1..65535" unless (1..65_535).cover?(port)

  port
rescue ArgumentError
  raise "Provider port is outside 1..65535"
end

def read_request(socket)
  request_line = socket.gets&.strip
  raise "client disconnected before sending a request" unless request_line
  raise "unexpected Provider route" unless request_line == "POST /v1/chat/completions HTTP/1.1"

  headers = {}
  while (line = socket.gets)
    line = line.chomp
    break if line.empty?

    name, value = line.split(":", 2)
    headers[name.downcase] = value.to_s.strip
  end
  length = Integer(headers.fetch("content-length"), 10)
  [headers, JSON.parse(socket.read(length))]
rescue JSON::ParserError, ArgumentError
  raise "request body is not valid bounded JSON"
end

def user_texts(body)
  body.fetch("messages").map do |message|
    message["content"] if message["role"] == "user" && message["content"].is_a?(String)
  end.compact
end

port = positive_port
secret = required_environment("AGENT_RUNTIME_TEST_PROVIDER_API_KEY")
ready_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_READY_FILE")
evidence_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE")
expected_input = required_environment("AGENT_RUNTIME_TEST_PROVIDER_INPUT")
final_text = required_environment("AGENT_RUNTIME_TEST_PROVIDER_FINAL_TEXT")
server = TCPServer.new("127.0.0.1", port)
FileUtils.touch(ready_file)

client = server.accept
headers, body = read_request(client)
raise "provider authorization failed" unless headers["authorization"] == "Bearer #{secret}"
raise "provider request must be streamed" unless body["stream"] == true
texts = user_texts(body)
raise "one-command input was not delivered exactly once" unless texts.count(expected_input) == 1

delta = {
  "choices" => [{ "index" => 0, "delta" => { "content" => final_text },
                   "finish_reason" => nil }]
}
finished = {
  "choices" => [{ "index" => 0, "delta" => {}, "finish_reason" => "stop" }],
  "usage" => { "prompt_tokens" => 11, "completion_tokens" => 5, "total_tokens" => 16 }
}
response = "data: #{JSON.generate(delta)}\n\n" \
           "data: #{JSON.generate(finished)}\n\n" \
           "data: [DONE]\n\n"
client.write("HTTP/1.1 200 OK\r\n")
client.write("Content-Type: text/event-stream; charset=utf-8\r\n")
client.write("Cache-Control: no-cache\r\n")
client.write("Content-Length: #{response.bytesize}\r\n")
client.write("Connection: close\r\n\r\n#{response}")
client.close
File.write(evidence_file, JSON.generate(
  "requests" => 1,
  "input_count" => texts.count(expected_input),
  "streamed" => body["stream"] == true
))
server.close
