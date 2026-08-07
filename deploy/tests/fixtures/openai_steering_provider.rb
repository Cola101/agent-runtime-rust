#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "socket"

MAX_REQUEST_BYTES = 1_048_576
STALE_OUTPUT = "STALE_PRE_STEER_OUTPUT"

def required_environment(name)
  value = ENV.fetch(name, "").strip
  raise "#{name} is required" if value.empty?

  value
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
    raise "malformed request header" unless value

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
  raise "provider messages are missing" unless body["messages"].is_a?(Array)
end

def user_texts(body)
  body.fetch("messages").map do |message|
    message["content"] if message["role"] == "user" && message["content"].is_a?(String)
  end.compact
end

def write_chunked_headers(socket)
  socket.write([
    "HTTP/1.1 200 OK",
    "Content-Type: text/event-stream; charset=utf-8",
    "Cache-Control: no-cache",
    "Transfer-Encoding: chunked",
    "Connection: close",
    "",
    ""
  ].join("\r\n"))
end

def write_chunk(socket, data)
  socket.write("#{data.bytesize.to_s(16)}\r\n#{data}\r\n")
  socket.flush
end

def stale_delta
  chunk = {
    "choices" => [{ "index" => 0, "delta" => { "content" => STALE_OUTPUT },
                    "finish_reason" => nil }]
  }
  "data: #{JSON.generate(chunk)}\n\n"
end

def final_response
  chunk = {
    "choices" => [{
      "index" => 0,
      "delta" => { "content" => "redirected instruction verified" },
      "finish_reason" => "stop"
    }],
    "usage" => { "prompt_tokens" => 24, "completion_tokens" => 4 }
  }
  body = "data: #{JSON.generate(chunk)}\n\ndata: [DONE]\n\n"
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

port = Integer(required_environment("AGENT_RUNTIME_TEST_PROVIDER_PORT"), 10)
raise "provider port is outside 1..65535" unless (1..65_535).cover?(port)

expected_bearer = required_environment("AGENT_RUNTIME_TEST_PROVIDER_API_KEY")
original_input = required_environment("AGENT_RUNTIME_TEST_PROVIDER_ORIGINAL_INPUT")
steering_input = required_environment("AGENT_RUNTIME_TEST_PROVIDER_STEERING_INPUT")
ready_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_READY_FILE")
first_request_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_FIRST_REQUEST_FILE")
evidence_file = required_environment("AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE")
server = TCPServer.new("127.0.0.1", port)
File.write(ready_file, "#{port}\n", mode: "w", perm: 0o600)
first_cancelled = false

begin
  first_socket = server.accept
  first_thread = Thread.new do
    begin
      headers, body = read_request(first_socket)
      validate_common!(headers, body, expected_bearer)
      raise "first request input changed" unless user_texts(body) == [original_input]

      write_chunked_headers(first_socket)
      write_chunk(first_socket, stale_delta)
      File.write(first_request_file, "ready\n", mode: "w", perm: 0o600)
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 30
      loop do
        raise "first model request was not cancelled" \
          if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline

        next unless IO.select([first_socket], nil, nil, 0.1)

        data = first_socket.recv_nonblock(1, exception: false)
        next if data == :wait_readable
        raise "client sent unexpected request data after the body" unless data.empty?

        first_cancelled = true
        break
      end
    rescue Errno::ECONNRESET, Errno::EPIPE
      first_cancelled = true
    ensure
      first_socket.close rescue nil
    end
  end

  second_socket = server.accept
  begin
    headers, body = read_request(second_socket)
    validate_common!(headers, body, expected_bearer)
    texts = user_texts(body)
    steering_input_count = texts.count(steering_input)
    raise "steered request lost or duplicated user input: #{texts}" \
      unless texts == [original_input, steering_input] && steering_input_count == 1
    raise "cancelled assistant output leaked into the next transcript" \
      if body.fetch("messages").to_s.include?(STALE_OUTPUT)

    second_socket.write(final_response)
  ensure
    second_socket.close rescue nil
  end
  first_thread.join
  evidence = {
    "requests" => 2,
    "first_request_cancelled" => first_cancelled,
    "steering_input_count" => 1,
    "stale_output_absent" => true
  }
  File.write(evidence_file, JSON.pretty_generate(evidence), mode: "w", perm: 0o600)
ensure
  server.close rescue nil
  File.unlink(ready_file) if File.exist?(ready_file)
  File.unlink(first_request_file) if File.exist?(first_request_file)
end
