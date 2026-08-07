#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "socket"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
APPROVE_LOCAL = File.join(ROOT, "deploy", "native", "approve-local")
APPROVAL_ID = "0198e899-e51e-7a0c-b3c3-07df25dfca45"
RUN_ID = "0198e899-e51e-7a0c-b3c3-07df25dfca46"
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
  body = socket.read(Integer(headers.fetch("content-length", "0"), 10))
  { request_line: request_line, headers: headers, body: body }
end

Dir.mktmpdir("agent-runtime-local-approval-test-") do |temporary|
  local_root = File.join(temporary, ".local")
  FileUtils.mkdir_p(File.join(local_root, "secrets"), mode: 0o700)
  File.write(File.join(local_root, ".agent-runtime-local-root"), "managed\n")
  token_path = File.join(local_root, "secrets", "local-access-token")
  File.write(token_path, "#{TOKEN}\n")
  FileUtils.chmod(0o600, token_path)

  server = TCPServer.new("127.0.0.1", 0)
  captured = Queue.new
  server_thread = Thread.new do
    client = server.accept
    captured << read_request(client)
    body = JSON.generate(
      "id" => APPROVAL_ID,
      "tenantId" => "11111111-1111-4111-8111-111111111111",
      "runId" => RUN_ID,
      "version" => 2,
      "status" => "approved",
      "createdAt" => "2026-08-02T00:00:00Z"
    )
    client.write("HTTP/1.1 200 OK\r\n")
    client.write("Content-Type: application/json\r\n")
    client.write("Content-Length: #{body.bytesize}\r\n")
    client.write("Connection: close\r\n\r\n#{body}")
    client.close
  end

  environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_LOCAL_API_PORT" => server.addr[1].to_s
  }
  output, error, status = Open3.capture3(
    environment,
    APPROVE_LOCAL,
    APPROVAL_ID,
    "1",
    "allow_once",
    chdir: ROOT
  )
  server_thread.join(2)
  server.close
  raise "local approval failed: #{output}#{error}" unless status.success?
  raise "approval result was not shown" unless output.include?("#{APPROVAL_ID}: approved (version 2)")
  raise "local token leaked to output" if "#{output}#{error}".include?(TOKEN)

  request = captured.pop
  unless request[:request_line] == "POST /v1/approvals/#{APPROVAL_ID}:decide HTTP/1.1"
    raise "approval used the wrong route: #{request[:request_line]}"
  end
  raise "approval omitted its bearer token" unless request.dig(:headers, "authorization") == "Bearer #{TOKEN}"
  raise "approval omitted JSON content type" unless request.dig(:headers, "content-type") == "application/json"
  expected = { "version" => 1, "decision" => "allow_once", "reason" => "native development review" }
  raise "approval body is invalid" unless JSON.parse(request[:body]) == expected

  output, error, status = Open3.capture3(
    {
      "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
      "AGENT_RUNTIME_CONTROL_API" => "https://api.example.com"
    },
    APPROVE_LOCAL,
    APPROVAL_ID,
    chdir: ROOT
  )
  raise "remote approval API was accepted" if status.success?
  unless error.include?("local approval command only accepts a loopback control API")
    raise "remote approval returned the wrong error: #{output}#{error}"
  end

  _output, error, status = Open3.capture3(
    { "AGENT_RUNTIME_LOCAL_ROOT" => local_root },
    APPROVE_LOCAL,
    APPROVAL_ID,
    "1",
    "allow_always",
    chdir: ROOT
  )
  raise "persistent approval was accepted by a local-only command" if status.success?
  raise "invalid decision returned the wrong error" unless error.include?("allow_once or deny")
end

puts "validated secure native approval decision"
