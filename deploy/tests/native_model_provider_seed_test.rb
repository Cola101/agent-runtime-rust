#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "socket"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
CONFIGURER = File.join(ROOT, "deploy", "native", "configure-local-model-provider")
PROVIDER_ID = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
TOKEN = "local.header.payload.signature"
SECRET = "local-provider-secret"

def read_request(socket)
  request_line = socket.gets&.strip
  raise "provider configurator disconnected before sending a request" unless request_line

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

Dir.mktmpdir("agent-runtime-provider-seed-test-") do |temporary|
  raise "native model Provider configurator is missing" unless File.executable?(CONFIGURER)

  local_root = File.join(temporary, ".local")
  secrets = File.join(local_root, "secrets")
  fake_bin = File.join(temporary, "bin")
  FileUtils.mkdir_p([File.join(local_root, "env"), secrets, fake_bin])
  File.write(File.join(local_root, ".agent-runtime-local-root"), "managed\n")
  File.write(File.join(local_root, "env", "native.env"), <<~ENVIRONMENT)
    export SPRING_DATASOURCE_URL='jdbc:postgresql://127.0.0.1:54329/agent_runtime'
    export SPRING_DATASOURCE_USERNAME='agent_runtime_owner'
    export SPRING_DATASOURCE_PASSWORD='postgres-secret'
  ENVIRONMENT
  {
    "provider-endpoint" => "http://127.0.0.1:45678/v1/chat/completions",
    "provider-model" => "native-test-model",
    "provider-api-key" => SECRET,
    "provider-protocol" => "openai_compatible",
    "local-access-token" => TOKEN
  }.each do |name, value|
    path = File.join(secrets, name)
    File.write(path, value)
    FileUtils.chmod(0o600, path)
  end

  captured_arguments = File.join(temporary, "psql-arguments")
  captured_sql = File.join(temporary, "binding.sql")
  psql = File.join(fake_bin, "psql")
  File.write(psql, <<~'SH')
    #!/bin/sh
    set -eu
    [ "$PGPASSWORD" = 'postgres-secret' ] || exit 91
    printf '%s\n' "$*" > "$CAPTURED_ARGUMENTS"
    while [ "$#" -gt 0 ]; do
      if [ "$1" = '-f' ]; then cp "$2" "$CAPTURED_SQL"; exit 0; fi
      shift
    done
    exit 92
  SH
  FileUtils.chmod(0o755, psql)

  server = TCPServer.new("127.0.0.1", 0)
  requests = Queue.new
  server_error = Queue.new
  server_thread = Thread.new do
    client = server.accept
    requests << read_request(client)
    body = JSON.generate(
      "id" => PROVIDER_ID,
      "name" => "Native Local Provider test",
      "protocol" => "openai_compatible",
      "endpoint" => "http://127.0.0.1:45678/v1/chat/completions",
      "model" => "native-test-model",
      "state" => "active",
      "credential_status" => "configured"
    )
    client.write("HTTP/1.1 201 Created\r\n")
    client.write("Content-Type: application/json\r\n")
    client.write("Content-Length: #{body.bytesize}\r\n")
    client.write("Connection: close\r\n\r\n#{body}")
    client.close
  rescue StandardError => error
    server_error << error
  end

  output, error, status = Open3.capture3(
    {
      "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
      "AGENT_RUNTIME_LOCAL_API_PORT" => server.addr[1].to_s,
      "AGENT_RUNTIME_PSQL_BIN" => psql,
      "SPRING_DATASOURCE_URL" => "jdbc:postgresql://127.0.0.1:54329/agent_runtime",
      "SPRING_DATASOURCE_USERNAME" => "agent_runtime_owner",
      "SPRING_DATASOURCE_PASSWORD" => "postgres-secret",
      "CAPTURED_ARGUMENTS" => captured_arguments,
      "CAPTURED_SQL" => captured_sql
    },
    CONFIGURER,
    chdir: ROOT
  )
  server_thread.join(2)
  raise server_error.pop unless server_error.empty?
  raise "native model Provider configuration failed: #{output}#{error}" unless status.success?
  raise "Provider credential leaked to output" if "#{output}#{error}".include?(SECRET)

  request = requests.pop
  unless request.fetch(:request_line) == "POST /v1/model-providers HTTP/1.1"
    raise "Provider was sent to the wrong API route: #{request.fetch(:request_line)}"
  end
  unless request.dig(:headers, "authorization") == "Bearer #{TOKEN}"
    raise "Provider configuration omitted the local bearer token"
  end
  body = JSON.parse(request.fetch(:body))
  unless body.slice("protocol", "endpoint", "model", "api_key") == {
    "protocol" => "openai_compatible",
    "endpoint" => "http://127.0.0.1:45678/v1/chat/completions",
    "model" => "native-test-model",
    "api_key" => SECRET
  }
    raise "Provider configuration did not preserve the native model settings"
  end
  unless body.fetch("name").start_with?("Native Local Provider ")
    raise "Provider configuration did not use a bounded local Provider name"
  end

  arguments = File.read(captured_arguments)
  raise "database password leaked into argv" if arguments.include?("postgres-secret")
  raise "Provider credential leaked into database argv" if arguments.include?(SECRET)
  unless arguments.include?("provider_id=#{PROVIDER_ID}")
    raise "sealed Provider id was not passed to the policy binding transaction"
  end
  sql = File.read(captured_sql)
  raise "Provider credential leaked into binding SQL" if sql.include?(SECRET)
  raise "binding must set the RLS tenant context" unless sql.include?("set_config('app.tenant_id'")
  raise "binding must be transactional" unless sql.include?("begin;") && sql.include?("commit;")
  unless sql.include?("insert into model_policy_candidates") &&
         sql.include?("delete from model_policy_candidates") &&
         sql.include?("delete from model_providers")
    raise "binding must atomically replace the local policy candidate and collect stale Providers"
  end
ensure
  server&.close
  server_thread&.kill if server_thread&.alive?
  server_thread&.join
end

puts "validated API-sealed native model Provider and atomic policy binding"
