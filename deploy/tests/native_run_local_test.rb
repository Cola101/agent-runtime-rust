#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "socket"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
RUN_LOCAL = File.join(ROOT, "deploy", "native", "run-local")
SESSION_ID = "77777777-7777-4777-8777-777777777777"
RUN_ID = "0198e899-e51e-7a0c-b3c3-07df25dfca45"
TOKEN = "local.header.payload.signature"

def read_request(socket)
  request_line = socket.gets&.strip
  raise "client disconnected before sending a request" unless request_line

  headers = {}
  while (line = socket.gets)
    line = line.chomp
    break if line.empty?

    name, value = line.split(":", 2)
    headers[name.downcase] = value.to_s.strip
  end
  body = socket.read(headers.fetch("content-length", "0").to_i)
  { request_line: request_line, headers: headers, body: body }
end

def write_response(socket, status:, content_type:, body:)
  socket.write("HTTP/1.1 #{status}\r\n")
  socket.write("Content-Type: #{content_type}\r\n")
  socket.write("Content-Length: #{body.bytesize}\r\n")
  socket.write("Connection: close\r\n\r\n")
  socket.write(body)
end

def run_against_server(local_root, responses, *arguments)
  server = TCPServer.new("127.0.0.1", 0)
  requests = Queue.new
  server_error = Queue.new
  server_thread = Thread.new do
    responses.each do |response|
      client = server.accept
      request = read_request(client)
      requests << request
      write_response(client, **response)
      client.close
    end
  rescue IOError, Errno::EBADF
    nil
  rescue StandardError => error
    server_error << error
  end

  environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_LOCAL_API_PORT" => server.addr[1].to_s,
    "AGENT_RUNTIME_RUN_TIMEOUT_SECONDS" => "5"
  }
  output, error, status = Open3.capture3(
    environment, "/usr/bin/ruby", RUN_LOCAL, *arguments, chdir: ROOT
  )
  server_thread.join(2)
  raise server_error.pop unless server_error.empty?

  [output, error, status, requests]
ensure
  server&.close
  server_thread&.kill if server_thread&.alive?
  server_thread&.join
end

Dir.mktmpdir("agent-runtime-local-run-test-") do |temporary|
  local_root = File.join(temporary, ".local")
  FileUtils.mkdir_p(File.join(local_root, "secrets"), mode: 0o700)
  File.write(File.join(local_root, ".agent-runtime-local-root"), "managed\n")
  token_path = File.join(local_root, "secrets", "local-access-token")
  File.write(token_path, "#{TOKEN}\n")
  FileUtils.chmod(0o600, token_path)

  accepted = JSON.generate(
    "run_id" => RUN_ID,
    "events_url" => "/v1/runs/#{RUN_ID}/events"
  )
  events = <<~SSE
    id:0198e899-e51e-7a0c-b3c3-07df25dfca46
    event:run.started
    data:{"status":"running"}

    id:0198e899-e51e-7a0c-b3c3-07df25dfca47
    event:model.text_delta
    data:{"delta":"hello"}

    id:0198e899-e51e-7a0c-b3c3-07df25dfca48
    event:run.succeeded
    data:{"status":"succeeded"}

  SSE
  responses = [
    { status: "202 Accepted", content_type: "application/json", body: accepted },
    { status: "200 OK", content_type: "text/event-stream", body: events }
  ]

  output, error, status, requests = run_against_server(
    local_root, responses, "Explain fencing tokens"
  )
  raise "local Run failed: #{output}#{error}" unless status.success?
  raise "accepted Run id was not shown" unless output.include?("Run accepted: #{RUN_ID}")
  raise "started event was not shown" unless output.include?("run.started")
  raise "model delta was not shown" unless output.include?("hello")
  raise "terminal event was not shown" unless output.include?("run.succeeded")
  raise "local access token leaked to output" if "#{output}#{error}".include?(TOKEN)

  post = requests.pop
  get = requests.pop
  unless post.fetch(:request_line) == "POST /v1/sessions/#{SESSION_ID}/runs HTTP/1.1"
    raise "Run was posted to the wrong route: #{post.fetch(:request_line)}"
  end
  raise "Run creation omitted its bearer token" unless post.dig(:headers, "authorization") == "Bearer #{TOKEN}"
  raise "Run creation omitted JSON content type" unless post.dig(:headers, "content-type") == "application/json"
  idempotency_key = post.dig(:headers, "idempotency-key")
  raise "Run idempotency key was not a UUID" unless idempotency_key&.match?(/\A[0-9a-f-]{36}\z/)
  expected_body = {
    "agent_version_id" => "66666666-6666-4666-8666-666666666666",
    "workspace_id" => "44444444-4444-4444-8444-444444444444",
    "model_policy_id" => "88888888-8888-4888-8888-888888888888",
    "input" => "Explain fencing tokens",
    "priority" => "interactive",
    "placement" => "auto",
    "budget" => {
      "max_tokens" => 4096,
      "max_cost_cents" => 100,
      "max_duration_seconds" => 600
    }
  }
  raise "Run request did not match the native seed contract" unless JSON.parse(post.fetch(:body)) == expected_body

  unless get.fetch(:request_line) == "GET /v1/runs/#{RUN_ID}/events HTTP/1.1"
    raise "Run events were read from the wrong route: #{get.fetch(:request_line)}"
  end
  raise "event stream omitted its bearer token" unless get.dig(:headers, "authorization") == "Bearer #{TOKEN}"
  raise "event stream omitted its media type" unless get.dig(:headers, "accept") == "text/event-stream"

  incomplete_events = <<~SSE
    event:run.started
    data:{"status":"running"}

  SSE
  incomplete_responses = [
    { status: "202 Accepted", content_type: "application/json", body: accepted },
    { status: "200 OK", content_type: "text/event-stream", body: incomplete_events }
  ]
  output, error, status, = run_against_server(local_root, incomplete_responses, "Incomplete stream")
  raise "an event stream without a terminal event was accepted" if status.success?
  unless error.include?("event stream ended before the Run reached a terminal state")
    raise "incomplete event stream returned the wrong error: #{output}#{error}"
  end

  output, error, status = Open3.capture3(
    {
      "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
      "AGENT_RUNTIME_CONTROL_API" => "https://api.example.com"
    },
    "/usr/bin/ruby", RUN_LOCAL, "Remote API", chdir: ROOT
  )
  raise "remote control API was accepted" if status.success?
  unless error.include?("local Run command only accepts a loopback control API")
    raise "remote control API returned the wrong error: #{output}#{error}"
  end

  output, error, status = Open3.capture3(
    { "AGENT_RUNTIME_LOCAL_ROOT" => local_root },
    "/usr/bin/ruby", RUN_LOCAL, chdir: ROOT
  )
  raise "missing Run input was accepted" if status.success?
  unless error.include?("AGENT_RUNTIME_RUN_INPUT='your request' make dev-run")
    raise "missing Run input advertised the wrong one-command usage: #{output}#{error}"
  end
end

puts "validated secure native Run creation and terminal SSE streaming"
