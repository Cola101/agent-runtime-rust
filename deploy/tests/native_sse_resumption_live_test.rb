#!/usr/bin/env ruby
# frozen_string_literal: true
#
# SSE event resumption against a live native stack.
#
# Why this test exists as a live test and not a unit test: resumption can only
# be observed by a client that *disconnects* mid-Run and reconnects. Every
# cheaper layer already passes while resumption is broken --
# `RunEventControllerTest` proves the header reaches the service, and
# `JdbcRunRepositoryIntegrationTest#eventReplayStartsStrictlyAfterLastEventId`
# proves the repository query, yet neither notices if the stream never honours
# the cursor across a real reconnect. The 500-on-unknown-cursor defect
# (docs/evidence/2026-08-07-sse-event-resumption.md) was found exactly here and
# was invisible to both.
#
# The load-bearing assertion is that the two captures are DISJOINT. A server
# that ignored Last-Event-ID and replayed from the beginning would still produce
# a second capture ending on the terminal event, so "it ended correctly" proves
# nothing on its own.

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
TOOL_PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "openai_tool_provider.rb")
TENANT_ID = "11111111-1111-4111-8111-111111111111"
AGENT_INSTRUCTIONS = "Inspect durable workspace evidence before acting; report only verified results."
SKILL_NAME = "sse-resumption-review"
SKILL_VERSION = "1.0.0"
SKILL_INSTRUCTIONS = "Read the bounded workspace fixture before answering."
EFFECTIVE_SYSTEM_INSTRUCTIONS = "#{AGENT_INSTRUCTIONS}\n\n" \
  "[Skill #{SKILL_NAME}@#{SKILL_VERSION}]\n#{SKILL_INSTRUCTIONS}"
WORKSPACE_FIXTURE = "Agent Runtime native workspace: trusted read-only Tool fixture."
# Captured before the approval is granted, so the remaining events do not exist
# yet when the client disconnects.
CAPTURE_A_EVENTS = 3
TERMINAL_EVENTS = %w[
  run.succeeded run.failed run.cancelled run.timed_out run.indeterminate
].freeze

raise "trusted Tool Provider fixture is missing" unless File.executable?(TOOL_PROVIDER)

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

  detail = Utf8Diagnostics.join(output, error, separator: "")
  raise "#{command.join(' ')} failed (#{status.exitstatus}):\n#{detail}"
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

def parse_json_response(response, expected_code)
  unless response.code == expected_code.to_s
    raise "control API returned HTTP #{response.code}: #{response.body.to_s.byteslice(0, 2_000)}"
  end
  JSON.parse(response.body)
rescue JSON::ParserError
  raise "control API returned malformed JSON"
end

def create_resource(api_port, token, path, body)
  request = authorized_request(Net::HTTP::Post, path, token, body)
  parse_json_response(direct_http(api_port, request), 201)
end

def psql_json(local_root, postgres_port, sql)
  password = File.read(File.join(local_root, "secrets", "postgres-password")).strip
  output, error, status = Open3.capture3(
    { "PGPASSWORD" => password },
    "psql", "-h", "127.0.0.1", "-p", postgres_port.to_s,
    "-U", "agent_runtime_owner", "-d", "agent_runtime",
    "-Atq", "-v", "ON_ERROR_STOP=1",
    "-c", "set app.tenant_id='#{TENANT_ID}'",
    "-c", sql
  )
  raise "PostgreSQL evidence query failed: #{error}" unless status.success?
  value = output.strip
  return nil if value.empty?

  JSON.parse(value.lines.last)
end

def pending_approval(api_port, token, run_id)
  response = direct_http(
    api_port, authorized_request(Net::HTTP::Get, "/v1/approvals?status=pending&limit=50", token)
  )
  Array(parse_json_response(response, 200)["items"]).find { |item| item["run_id"] == run_id }
end

def port_open?(port)
  socket = TCPSocket.new("127.0.0.1", port)
  socket.close
  true
rescue Errno::ECONNREFUSED, Errno::EHOSTUNREACH
  false
end

# Reads an SSE stream over a raw socket so the disconnect is ours to make.
# `stop_after` is the only reason a capture ends early; using a read timeout
# instead would make a truncated capture indistinguishable from a resumed one.
def capture_sse(port, run_id, token, last_event_id: nil, stop_after: 0, idle_seconds: 60)
  socket = TCPSocket.new("127.0.0.1", port)
  request = +"GET /v1/runs/#{run_id}/events HTTP/1.1\r\n"
  request << "Host: 127.0.0.1:#{port}\r\n"
  request << "Accept: text/event-stream\r\n"
  request << "Authorization: Bearer #{token}\r\n"
  request << "Last-Event-ID: #{last_event_id}\r\n" if last_event_id
  request << "Connection: close\r\n\r\n"
  socket.write(request)

  status = socket.gets.to_s.strip.split(" ")[1]
  until (header = socket.gets).nil?
    break if header == "\r\n" || header.chomp.empty?
  end
  events = []
  current = {}
  reason = "stream_closed"
  deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + idle_seconds

  while Process.clock_gettime(Process::CLOCK_MONOTONIC) < deadline
    next unless IO.select([socket], nil, nil, 1)

    line = socket.gets
    break if line.nil?

    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + idle_seconds
    line = line.chomp.delete_suffix("\r")
    if line.empty?
      next if current.empty?

      events << { "id" => current["id"], "type" => current["event"] }
      current = {}
      if stop_after.positive? && events.length >= stop_after
        reason = "client_disconnected"
        break
      end
      if TERMINAL_EVENTS.include?(events.last["type"])
        reason = "terminal_event"
        break
      end
      next
    end
    name, _, value = line.partition(":")
    current[name.strip] = value.strip unless name.strip.empty?
  end
  { "status" => status, "events" => events, "stop_reason" => reason }
ensure
  socket&.close
end

temporary = Dir.mktmpdir("agent-runtime-sse-resumption-live-")
local_root = File.join(temporary, ".local")
provider_ready = File.join(temporary, "provider-ready")
provider_evidence = File.join(temporary, "provider-evidence.json")
provider_log = File.join(temporary, "provider.log")
provider_secret = "sse-resumption-#{SecureRandom.hex(16)}"
ports = %i[provider postgres nats nats_monitor api management model_health model_grpc
           checkpoint_health checkpoint_grpc worker_health console].to_h do |name|
  [name, free_loopback_port]
end
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
  "AGENT_RUNTIME_PROVIDER_MODEL" => "local-sse-resumption-test",
  "AGENT_RUNTIME_PROVIDER_API_KEY" => provider_secret,
  # AGENT_RUNTIME_DOWNLOAD_PROXY is deliberately not set. Setting it overrides
  # `with-download-proxy`'s system-proxy resolution outright, so pinning a
  # literal endpoint here would pick the proxy for every machine that runs this
  # test and fail wherever that endpoint is absent. Letting the wrapper resolve
  # it keeps proxy selection in one place, which is the rule.
  "AGENT_RUNTIME_STARTUP_ATTEMPTS" => "900"
}
provider_pid = nil
runtime_started = false
failure = nil

begin
  provider_pid = Process.spawn(
    {
      "AGENT_RUNTIME_TEST_PROVIDER_PORT" => ports.fetch(:provider).to_s,
      "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => provider_secret,
      "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => provider_ready,
      "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => provider_evidence,
      "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_SYSTEM_INSTRUCTIONS" => EFFECTIVE_SYSTEM_INSTRUCTIONS
    },
    TOOL_PROVIDER, chdir: ROOT, out: provider_log, err: provider_log, pgroup: true
  )
  eventually("trusted Tool Provider") do
    File.file?(provider_ready) && process_alive?(provider_pid)
  end

  # Deliberately outside the timeout below. `supervisor clean` removes the shared
  # build outputs, so every run of this test compiles the Rust workspace, the
  # control plane and the Console from cold -- on this machine that alone exceeds
  # the budget any sane hang-detection timeout would use. The timeout is here to
  # catch a Run that stops making progress, not to race the compiler.
  run_command(environment, SUPERVISOR, "start")
  runtime_started = true

  Timeout.timeout(300) do
    token = File.read(File.join(local_root, "secrets", "local-access-token")).strip
    api = ports.fetch(:api)

    context = parse_json_response(
      direct_http(api, authorized_request(Net::HTTP::Get, "/v1/console/resource-context", token)),
      200
    )
    project_id = context.fetch("projects").first.fetch("id")
    workspace = create_resource(
      api, token, "/v1/workspaces", "project_id" => project_id, "name" => "SSE Resumption Workspace"
    )
    agent = create_resource(
      api, token, "/v1/agents",
      "workspace_id" => workspace.fetch("id"), "name" => "SSE Resumption Agent"
    )
    skill_version = create_resource(
      api, token, "/v1/skills:publish",
      "name" => SKILL_NAME,
      "semantic_version" => SKILL_VERSION,
      "description" => "Review bounded workspace evidence",
      "instructions" => SKILL_INSTRUCTIONS,
      "tool_names" => ["workspace.read_text"],
      "supported_platforms" => %w[darwin-arm64 linux-arm64 linux-x86_64],
      "min_runtime_version" => "0.1.0"
    )
    agent_version = create_resource(
      api, token, "/v1/agents/#{agent.fetch('id')}/versions",
      "instructions" => AGENT_INSTRUCTIONS,
      "delegated_scopes" => ["tool:workspace.read"],
      "skill_version_ids" => [skill_version.fetch("id")]
    )
    provider = create_resource(
      api, token, "/v1/model-providers",
      "name" => "SSE Resumption Provider",
      "protocol" => "openai_compatible",
      "endpoint" => "http://127.0.0.1:#{ports.fetch(:provider)}/v1/chat/completions",
      "model" => "local-sse-resumption-test",
      "api_key" => provider_secret
    )
    model_policy = create_resource(
      api, token, "/v1/model-policies",
      "workspace_id" => workspace.fetch("id"),
      "name" => "SSE Resumption Policy",
      "routing" => "ordered_failover",
      "provider_ids" => [provider.fetch("id")]
    )
    session = create_resource(
      api, token, "/v1/sessions",
      "workspace_id" => workspace.fetch("id"), "title" => "SSE resumption"
    )
    workspace_path = File.join(local_root, "state", "workspaces", TENANT_ID, workspace.fetch("id"))
    FileUtils.mkdir_p(workspace_path, mode: 0o700)
    FileUtils.chmod(0o700, [File.dirname(workspace_path), workspace_path])
    File.write(File.join(workspace_path, "README.txt"), "#{WORKSPACE_FIXTURE}\n",
               mode: "w", perm: 0o600)

    create = authorized_request(
      Net::HTTP::Post, "/v1/sessions/#{session.fetch('id')}/runs", token,
      {
        "agent_version_id" => agent_version.fetch("id"),
        "workspace_id" => workspace.fetch("id"),
        "model_policy_id" => model_policy.fetch("id"),
        "input" => "Read the trusted native workspace fixture and report its content.",
        "budget" => {
          "max_tokens" => 4_096, "max_cost_cents" => 100, "max_duration_seconds" => 120
        }
      }
    )
    create["Idempotency-Key"] = SecureRandom.uuid
    run_id = parse_json_response(direct_http(api, create), 202).fetch("run_id")

    # The Run parks here, so the events after the disconnect provably do not
    # exist yet while capture A is open.
    approval = eventually("persisted Tool approval") { pending_approval(api, token, run_id) }

    capture_a = capture_sse(api, run_id, token, stop_after: CAPTURE_A_EVENTS, idle_seconds: 45)
    unless capture_a.fetch("status") == "200"
      raise "first SSE capture failed with HTTP #{capture_a.fetch('status')}"
    end
    unless capture_a.fetch("stop_reason") == "client_disconnected"
      raise "first SSE capture ended by #{capture_a.fetch('stop_reason')} instead of a real disconnect"
    end
    unless capture_a.fetch("events").length == CAPTURE_A_EVENTS
      raise "first SSE capture did not observe #{CAPTURE_A_EVENTS} events: #{capture_a}"
    end
    cursor = capture_a.fetch("events").last.fetch("id")

    decision = authorized_request(
      Net::HTTP::Post, "/v1/approvals/#{approval.fetch('id')}:decide", token,
      "decision" => "allow_once", "version" => approval.fetch("version"),
      "reason" => "SSE resumption live test"
    )
    parse_json_response(direct_http(api, decision), 200)

    capture_b = capture_sse(api, run_id, token, last_event_id: cursor, idle_seconds: 90)
    unless capture_b.fetch("stop_reason") == "terminal_event"
      raise "resumed SSE capture never reached a terminal event: #{capture_b}"
    end

    a_ids = capture_a.fetch("events").map { |event| event.fetch("id") }
    b_ids = capture_b.fetch("events").map { |event| event.fetch("id") }

    # The decisive check. Replaying from zero would also end on a terminal event.
    overlap = a_ids & b_ids
    unless overlap.empty?
      raise "Last-Event-ID was not honoured; the resumed stream re-delivered #{overlap.inspect}"
    end

    authoritative = psql_json(local_root, ports.fetch(:postgres), <<~SQL)
      select json_agg(json_build_object('event_id',event_id,'sequence',sequence)
                      order by sequence)::text
        from run_events where tenant_id = '#{TENANT_ID}' and run_id = '#{run_id}'
    SQL
    expected_ids = authoritative.map { |event| event.fetch("event_id") }
    unless a_ids + b_ids == expected_ids
      raise "SSE delivery does not match the authoritative log: " \
            "streamed #{(a_ids + b_ids).length}, durable #{expected_ids.length}"
    end
    sequences = authoritative.map { |event| event.fetch("sequence") }
    unless sequences == (sequences.first..sequences.last).to_a
      raise "durable event sequence has a hole: #{sequences.inspect}"
    end
    unless capture_b.fetch("events").length > 1
      raise "resumed capture did not cover events created after the disconnect"
    end

    # Negative control: without a cursor the same Run replays in full. Without
    # this, a stream that simply stopped early would satisfy every check above.
    full = capture_sse(api, run_id, token, idle_seconds: 45)
    unless full.fetch("events").map { |event| event.fetch("id") } == expected_ids
      raise "uncursored replay did not return the whole Run: #{full.fetch('events').length}"
    end
    unless full.fetch("events").length == a_ids.length + b_ids.length
      raise "cursored and uncursored replay disagree on the Run length"
    end

    # An unknown cursor is client input. Left unmapped it surfaced as 500, and a
    # browser EventSource retries 5xx with the same unusable cursor forever.
    unknown = authorized_request(
      Net::HTTP::Get, "/v1/runs/#{run_id}/events", token
    )
    unknown["Accept"] = "text/event-stream"
    unknown["Last-Event-ID"] = SecureRandom.uuid
    rejected = direct_http(api, unknown)
    if rejected.code.start_with?("5")
      raise "unknown Last-Event-ID returned HTTP #{rejected.code}; EventSource will retry forever"
    end
    unless rejected.code == "404"
      raise "unknown Last-Event-ID returned HTTP #{rejected.code} instead of 404"
    end
    problem = JSON.parse(rejected.body)
    unless problem["type"] == "urn:agent-runtime:problem:event-cursor"
      raise "unknown Last-Event-ID lacks its distinguishable problem type: #{problem}"
    end

    provider_result = eventually("provider evidence", timeout: 10) do
      JSON.parse(File.read(provider_evidence)) if File.file?(provider_evidence)
    end
    unless provider_result["requests"] == 2 && provider_result["result_verified"] == true
      raise "trusted Tool Provider evidence is incomplete: #{provider_result}"
    end
  end
rescue StandardError, Timeout::Error => error
  failure = error
ensure
  if runtime_started || File.exist?(File.join(local_root, ".agent-runtime-local-root"))
    _output, error, status = Open3.capture3(environment, SUPERVISOR, "clean", chdir: ROOT)
    failure ||= RuntimeError.new("native clean failed: #{error}") unless status.success?
  end
  Process.kill("TERM", -provider_pid) rescue nil if provider_pid && process_alive?(provider_pid)
  Process.wait(provider_pid) rescue nil if provider_pid
  open_ports = ports.values.select { |port| port_open?(port) }
  failure ||= RuntimeError.new("SSE resumption test left ports open: #{open_ports}") unless open_ports.empty?
  failure ||= RuntimeError.new("SSE resumption test left local state behind") if File.exist?(local_root)
  if failure
    detail = File.file?(provider_log) ? File.read(provider_log).gsub(provider_secret, "[REDACTED]") : ""
    warn "#{failure.class}: #{failure.message}\n#{detail}"
  end
  FileUtils.remove_entry(temporary) if File.exist?(temporary)
end

raise failure if failure

puts "validated SSE event resumption across a real disconnect with complete cleanup"
