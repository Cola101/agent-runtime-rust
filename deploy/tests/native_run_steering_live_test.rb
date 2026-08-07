#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "json"
require "net/http"
require "open3"
require "securerandom"
require "socket"
require "timeout"
require "tmpdir"
require_relative "support/utf8_diagnostics"

ROOT = File.expand_path("../..", __dir__)
SUPERVISOR = File.join(ROOT, "deploy", "native", "supervisor")
DOWNLOAD_WRAPPER = File.join(ROOT, "deploy", "native", "with-download-proxy")
STEERING_PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "openai_steering_provider.rb")
TENANT_ID = "11111111-1111-4111-8111-111111111111"
SESSION_ID = "77777777-7777-4777-8777-777777777777"
AGENT_VERSION_ID = "66666666-6666-4666-8666-666666666666"
WORKSPACE_ID = "44444444-4444-4444-8444-444444444444"
MODEL_POLICY_ID = "88888888-8888-4888-8888-888888888888"
ORIGINAL_INPUT = "Inspect the original path before reporting."
STEERING_INPUT = "Stop that work and report only the redirected instruction."

raise "steering provider fixture is missing" unless File.executable?(STEERING_PROVIDER)

def free_loopback_port
  server = TCPServer.new("127.0.0.1", 0)
  server.addr[1]
ensure
  server&.close
end

def eventually(label, timeout: 45, interval: 0.2)
  deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout
  loop do
    value = yield
    return value if value
    raise "timed out waiting for #{label}" if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline

    sleep interval
  end
end

def process_alive?(pid)
  Process.kill(0, pid)
  true
rescue Errno::ESRCH
  false
end

def run_command(environment, *command)
  output, error, status = Open3.capture3(environment, *command, chdir: ROOT)
  return output if status.success?

  raise "#{command.join(' ')} failed (#{status.exitstatus}):\n" \
        "#{Utf8Diagnostics.join(output, error, separator: '')}"
end

def direct_http(port, request, timeout: 30)
  http = Net::HTTP.new("127.0.0.1", port, nil)
  http.open_timeout = 10
  http.read_timeout = timeout
  http.write_timeout = 10 if http.respond_to?(:write_timeout=)
  http.request(request)
end

def authorized_request(method, path, token, body = nil)
  request = method.new(path)
  request["Authorization"] = "Bearer #{token}"
  request["Accept"] = "application/json"
  if body
    request["Content-Type"] = "application/json"
    request.body = JSON.generate(body)
  end
  request
end

def parse_json(response, expected_code)
  raise "control API returned HTTP #{response.code}: #{response.body.to_s.byteslice(0, 2_000)}" \
    unless response.code == expected_code.to_s

  JSON.parse(response.body)
end

def run_status(api_port, token, run_id)
  response = direct_http(api_port, authorized_request(Net::HTTP::Get, "/v1/runs", token))
  Array(parse_json(response, 200)["items"]).find { |item| item["id"] == run_id }&.fetch("status")
end

def parse_sse_events(body)
  events = []
  event_type = nil
  event_id = nil
  data = []
  dispatch = lambda do
    unless event_type.nil? && event_id.nil? && data.empty?
      events << {
        "event_id" => event_id,
        "type" => event_type || "message",
        "payload" => data.empty? ? nil : JSON.parse(data.join("\n"))
      }
    end
    event_type = nil
    event_id = nil
    data = []
  end
  body.each_line do |raw_line|
    line = raw_line.chomp.delete_suffix("\r")
    if line.empty?
      dispatch.call
      next
    end
    next if line.start_with?(":")

    field, value = line.split(":", 2)
    value = value.to_s.sub(/\A /, "")
    case field
    when "id" then event_id = value
    when "event" then event_type = value
    when "data" then data << value
    end
  end
  dispatch.call
  events
rescue JSON::ParserError
  raise "control API returned malformed SSE event data"
end

def psql_json(local_root, postgres_port, sql)
  password = File.read(File.join(local_root, "secrets", "postgres-password")).strip
  output, error, status = Open3.capture3(
    { "PGPASSWORD" => password },
    "psql", "-h", "127.0.0.1", "-p", postgres_port.to_s,
    "-U", "agent_runtime_owner", "-d", "agent_runtime", "-Atq", "-v", "ON_ERROR_STOP=1",
    "-c", "set app.tenant_id='#{TENANT_ID}'", "-c", sql
  )
  raise "PostgreSQL evidence query failed: #{error}" unless status.success?

  value = output.strip.lines.last
  value ? JSON.parse(value) : nil
end

def rss_kib(local_root)
  pgids = Dir.glob(File.join(local_root, "run", "*.pgid")).map do |path|
    Integer(File.read(path).strip, 10)
  rescue ArgumentError
    nil
  end.compact
  postgres_root = File.join(local_root, "state", "postgres")
  output, status = Open3.capture2("ps", "-axo", "pgid=,rss=,command=")
  raise "failed to inspect native runtime memory" unless status.success?

  output.lines.sum do |line|
    pgid, rss, command = line.strip.split(/\s+/, 3)
    next 0 unless pgid && rss && command
    next 0 unless pgids.include?(Integer(pgid, 10)) || command.include?(postgres_root)

    Integer(rss, 10)
  rescue ArgumentError
    0
  end
end

def port_open?(port)
  socket = TCPSocket.new("127.0.0.1", port)
  socket.close
  true
rescue Errno::ECONNREFUSED, Errno::EHOSTUNREACH
  false
end

temporary = Dir.mktmpdir("agent-runtime-steering-live-")
local_root = File.join(temporary, ".local")
provider_ready = File.join(temporary, "provider-ready")
first_request_ready = File.join(temporary, "first-request-ready")
provider_evidence = File.join(temporary, "provider-evidence.json")
provider_log = File.join(temporary, "provider.log")
provider_secret = "steering-#{SecureRandom.hex(16)}"
ports = %i[provider postgres nats nats_monitor api management model_health model_grpc
           checkpoint_health checkpoint_grpc worker_health console].to_h do |name|
  [name, free_loopback_port]
end
provider_pid = nil
runtime_started = false
failure = nil
runtime_diagnostics = ""
environment = {
  "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
  "AGENT_RUNTIME_LOCAL_POSTGRES_PORT" => ports.fetch(:postgres).to_s,
  "AGENT_RUNTIME_LOCAL_NATS_PORT" => ports.fetch(:nats).to_s,
  "AGENT_RUNTIME_LOCAL_NATS_MONITOR_PORT" => ports.fetch(:nats_monitor).to_s,
  "AGENT_RUNTIME_LOCAL_API_PORT" => ports.fetch(:api).to_s,
  "AGENT_RUNTIME_LOCAL_MANAGEMENT_PORT" => ports.fetch(:management).to_s,
  "AGENT_RUNTIME_LOCAL_MODEL_HEALTH_PORT" => ports.fetch(:model_health).to_s,
  "AGENT_RUNTIME_LOCAL_MODEL_GRPC_PORT" => ports.fetch(:model_grpc).to_s,
  "AGENT_RUNTIME_LOCAL_CHECKPOINT_HEALTH_PORT" => ports.fetch(:checkpoint_health).to_s,
  "AGENT_RUNTIME_LOCAL_CHECKPOINT_GRPC_PORT" => ports.fetch(:checkpoint_grpc).to_s,
  "AGENT_RUNTIME_LOCAL_WORKER_HEALTH_PORT" => ports.fetch(:worker_health).to_s,
  "AGENT_RUNTIME_LOCAL_CONSOLE_PORT" => ports.fetch(:console).to_s,
  "AGENT_RUNTIME_PROVIDER_ENDPOINT" =>
    "http://127.0.0.1:#{ports.fetch(:provider)}/v1/chat/completions",
  "AGENT_RUNTIME_PROVIDER_MODEL" => "local-steering-test",
  "AGENT_RUNTIME_PROVIDER_API_KEY" => provider_secret,
  "AGENT_RUNTIME_STARTUP_ATTEMPTS" => "900"
}

begin
  Timeout.timeout(300) do
    provider_pid = Process.spawn(
      {
        "AGENT_RUNTIME_TEST_PROVIDER_PORT" => ports.fetch(:provider).to_s,
        "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => provider_secret,
        "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => provider_ready,
        "AGENT_RUNTIME_TEST_PROVIDER_FIRST_REQUEST_FILE" => first_request_ready,
        "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => provider_evidence,
        "AGENT_RUNTIME_TEST_PROVIDER_ORIGINAL_INPUT" => ORIGINAL_INPUT,
        "AGENT_RUNTIME_TEST_PROVIDER_STEERING_INPUT" => STEERING_INPUT
      },
      STEERING_PROVIDER, chdir: ROOT, out: provider_log, err: provider_log, pgroup: true
    )
    eventually("steering provider", timeout: 10) do
      File.file?(provider_ready) && process_alive?(provider_pid)
    end

    run_command(environment, SUPERVISOR, "start")
    runtime_started = true
    token = File.read(File.join(local_root, "secrets", "local-access-token")).strip
    create = authorized_request(
      Net::HTTP::Post, "/v1/sessions/#{SESSION_ID}/runs", token,
      "agent_version_id" => AGENT_VERSION_ID,
      "workspace_id" => WORKSPACE_ID,
      "model_policy_id" => MODEL_POLICY_ID,
      "input" => ORIGINAL_INPUT,
      "budget" => {
        "max_tokens" => 4_096, "max_cost_cents" => 100, "max_duration_seconds" => 120
      }
    )
    create["Idempotency-Key"] = SecureRandom.uuid
    accepted = parse_json(direct_http(ports.fetch(:api), create), 202)
    run_id = accepted.fetch("run_id")
    eventually("blocked first model request", timeout: 45) do
      current_status = run_status(ports.fetch(:api), token, run_id)
      if %w[succeeded failed cancelled timed_out indeterminate].include?(current_status)
        raise "Run reached #{current_status} before the steering boundary"
      end
      File.file?(first_request_ready) && current_status == "running"
    end

    browser_output = File.join(temporary, "playwright")
    run_command(
      environment.merge(
        "AGENT_RUNTIME_LIVE_CONSOLE_URL" => "http://127.0.0.1:#{ports.fetch(:console)}",
        "AGENT_RUNTIME_LIVE_BROWSER_OUTPUT_DIR" => browser_output,
        "AGENT_RUNTIME_LIVE_RUN_ID" => run_id,
        "AGENT_RUNTIME_LIVE_STEERING_INPUT" => STEERING_INPUT,
        "AGENT_RUNTIME_LIVE_BEFORE_SCREENSHOT" =>
          File.join(ROOT, "docs", "evidence", "2026-08-02-native-steering-running.png"),
        "AGENT_RUNTIME_LIVE_AFTER_SCREENSHOT" =>
          File.join(ROOT, "docs", "evidence", "2026-08-02-native-steering-succeeded.png")
      ),
      DOWNLOAD_WRAPPER, "pnpm", "--filter", "@agent-runtime/console", "exec", "playwright",
      "test", "e2e-live/native-steering.spec.ts", "--config", "playwright.native-live.config.ts"
    )

    eventually("steered Run terminal state", timeout: 20) do
      run_status(ports.fetch(:api), token, run_id) == "succeeded"
    end
    provider_result = eventually("provider cancellation evidence", timeout: 10) do
      JSON.parse(File.read(provider_evidence)) if File.file?(provider_evidence)
    end
    unless provider_result == {
      "requests" => 2, "first_request_cancelled" => true,
      "steering_input_count" => 1, "stale_output_absent" => true
    }
      raise "provider did not prove model-stream replacement: #{provider_result}"
    end

    events_request = authorized_request(Net::HTTP::Get, "/v1/runs/#{run_id}/events", token)
    events_request["Accept"] = "text/event-stream"
    events = parse_sse_events(direct_http(ports.fetch(:api), events_request).body)
    event_types = events.map { |event| event.fetch("type") }
    unless event_types.include?("run.steer.applied") && event_types.last == "run.succeeded"
      raise "steering SSE evidence is incomplete: #{event_types}"
    end
    applied_index = event_types.index("run.steer.applied")
    stale_after_steer = events.drop(applied_index + 1).any? do |event|
      event.fetch("payload").to_s.include?("STALE_PRE_STEER_OUTPUT")
    end
    if stale_after_steer
      raise "cancelled model output leaked after the durable steering boundary"
    end

    ledger = psql_json(local_root, ports.fetch(:postgres), <<~SQL)
      select json_build_object(
        'commands',count(*),
        'applied',count(*) filter (where state = 'applied'),
        'applied_events',count(applied_event_id),
        'pending',count(*) filter (where state = 'pending')
      )::text
        from run_steering_commands
       where tenant_id = '#{TENANT_ID}' and run_id = '#{run_id}'
    SQL
    unless ledger == { "commands" => 1, "applied" => 1, "applied_events" => 1, "pending" => 0 }
      raise "steering ledger did not converge exactly once: #{ledger}"
    end
    resident_kib = rss_kib(local_root)
    raise "native runtime exceeded 4 GiB RSS: #{resident_kib} KiB" \
      unless resident_kib.positive? && resident_kib < 4 * 1024 * 1024

    puts JSON.generate(
      "run_id" => run_id, "steering" => "applied", "provider" => provider_result,
      "rss_kib" => resident_kib, "events" => event_types
    )
  end
rescue StandardError, Timeout::Error => error
  failure = error
ensure
  if failure && File.directory?(File.join(local_root, "logs"))
    runtime_diagnostics = Dir.glob(File.join(local_root, "logs", "*.log")).sort.map do |path|
      contents = File.binread(path)
      tail = contents.byteslice([contents.bytesize - 12_000, 0].max, 12_000)
      Utf8Diagnostics.join("--- #{File.basename(path)} ---", tail)
    rescue StandardError => error
      "--- #{File.basename(path)} unavailable: #{error.message} ---"
    end.join("\n")
  end
  if runtime_started
    run_command(environment, SUPERVISOR, "clean")
  end
  if provider_pid && process_alive?(provider_pid)
    Process.kill("TERM", -provider_pid) rescue nil
  end
  Process.wait(provider_pid) rescue nil if provider_pid
  open_ports = ports.values.select { |port| port_open?(port) }
  failure ||= RuntimeError.new("native steering test left ports open: #{open_ports}") unless open_ports.empty?
  failure ||= RuntimeError.new("native steering test left local state behind") if File.exist?(local_root)
  if failure
    detail = if File.file?(provider_log)
               Utf8Diagnostics.normalize(File.binread(provider_log)).gsub(provider_secret, "[REDACTED]")
             else
               ""
             end
    diagnostics = runtime_diagnostics.gsub(provider_secret, "[REDACTED]")
    warn Utf8Diagnostics.join("#{failure.class}: #{failure.message}", detail, diagnostics)
  end
  FileUtils.remove_entry(temporary) if File.exist?(temporary)
end

raise failure if failure

puts "validated real-browser native Run steering and old-stream cancellation"
