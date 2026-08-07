#!/usr/bin/env ruby
# frozen_string_literal: true
#
# The Shell Tool against a live native stack.
#
# This gate exists because `shell.exec` is the only way to observe the
# container from inside it. Seatbelt receives its policy as argv on a process
# that lives for milliseconds, so ADR-0036, ADR-0037 and ADR-0038 were each
# verified by unit tests that spawn `sandbox-exec` directly -- real, but not the
# production path. A command running inside the Worker's own container answers
# the question the argv never could, and this gate keeps that answer from going
# stale the way the 2026-08-02 evidence did.
#
# Nothing here prints credential content: the probe redirects every credential
# read to /dev/null and emits only a verdict.

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
PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "shell_probe_provider.rb")
TENANT_ID = "11111111-1111-4111-8111-111111111111"
AGENT_INSTRUCTIONS = "Inspect durable workspace evidence before acting; report only verified results."
SKILL_NAME = "shell-containment-probe"
SKILL_VERSION = "1.0.0"
SKILL_INSTRUCTIONS = "Run the containment probe and report what it observed."
ESCAPE_MARKER = "/tmp/agent-shell-gate-escape"

raise "shell probe Provider fixture is missing" unless File.executable?(PROVIDER)

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
  parse_json_response(direct_http(api_port, authorized_request(Net::HTTP::Post, path, token, body)), 201)
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

# The probe emits `key=value` lines; this turns them into a hash so assertions
# name what they check rather than matching substrings against one blob.
def parse_probe(stdout)
  # `filter_map` needs Ruby 2.7; the system Ruby here is 2.6.
  stdout.lines.map do |line|
    key, separator, value = line.strip.partition("=")
    [key, value] unless separator.empty?
  end.compact.to_h
end

temporary = Dir.mktmpdir("agent-runtime-shell-live-")
local_root = File.join(temporary, ".local")
provider_ready = File.join(temporary, "provider-ready")
provider_evidence = File.join(temporary, "provider-evidence.json")
provider_log = File.join(temporary, "provider.log")
provider_secret = "shell-gate-#{SecureRandom.hex(16)}"
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
  "AGENT_RUNTIME_PROVIDER_MODEL" => "local-shell-gate-test",
  "AGENT_RUNTIME_PROVIDER_API_KEY" => provider_secret,
  # AGENT_RUNTIME_DOWNLOAD_PROXY is deliberately unset: setting it overrides
  # `with-download-proxy`'s system-proxy resolution outright, so pinning a
  # literal endpoint here would fail on every machine without that endpoint.
  "AGENT_RUNTIME_STARTUP_ATTEMPTS" => "900"
}
provider_pid = nil
runtime_started = false
failure = nil

begin
  FileUtils.rm_f(ESCAPE_MARKER)
  provider_pid = Process.spawn(
    {
      "AGENT_RUNTIME_TEST_PROVIDER_PORT" => ports.fetch(:provider).to_s,
      "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => provider_secret,
      "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => provider_ready,
      "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => provider_evidence
    },
    PROVIDER, chdir: ROOT, out: provider_log, err: provider_log, pgroup: true
  )
  eventually("shell probe Provider") { File.file?(provider_ready) && process_alive?(provider_pid) }

  # Outside the timeout below: `supervisor clean` removes the shared build
  # outputs, so every run compiles from cold. The timeout is here to catch a Run
  # that stops making progress, not to race the compiler.
  run_command(environment, SUPERVISOR, "start")
  runtime_started = true

  Timeout.timeout(300) do
    token = File.read(File.join(local_root, "secrets", "local-access-token")).strip
    api = ports.fetch(:api)

    context = parse_json_response(
      direct_http(api, authorized_request(Net::HTTP::Get, "/v1/console/resource-context", token)), 200
    )
    project_id = context.fetch("projects").first.fetch("id")
    workspace = create_resource(
      api, token, "/v1/workspaces", "project_id" => project_id, "name" => "Shell Gate Workspace"
    )
    agent = create_resource(
      api, token, "/v1/agents", "workspace_id" => workspace.fetch("id"), "name" => "Shell Gate Agent"
    )
    skill_version = create_resource(
      api, token, "/v1/skills:publish",
      "name" => SKILL_NAME,
      "semantic_version" => SKILL_VERSION,
      "description" => "Probe the container from inside it",
      "instructions" => SKILL_INSTRUCTIONS,
      "tool_names" => ["shell.exec"],
      "supported_platforms" => %w[darwin-arm64 linux-arm64 linux-x86_64],
      "min_runtime_version" => "0.1.0"
    )
    agent_version = create_resource(
      api, token, "/v1/agents/#{agent.fetch('id')}/versions",
      "instructions" => AGENT_INSTRUCTIONS,
      # Shell only. The file Tools are installed on the Worker, so the model
      # seeing them would mean the scope intersection leaked.
      "delegated_scopes" => ["tool:shell.exec"],
      "skill_version_ids" => [skill_version.fetch("id")]
    )
    provider = create_resource(
      api, token, "/v1/model-providers",
      "name" => "Shell Gate Provider",
      "protocol" => "openai_compatible",
      "endpoint" => "http://127.0.0.1:#{ports.fetch(:provider)}/v1/chat/completions",
      "model" => "local-shell-gate-test",
      "api_key" => provider_secret
    )
    model_policy = create_resource(
      api, token, "/v1/model-policies",
      "workspace_id" => workspace.fetch("id"),
      "name" => "Shell Gate Policy",
      "routing" => "ordered_failover",
      "provider_ids" => [provider.fetch("id")]
    )
    session = create_resource(
      api, token, "/v1/sessions",
      "workspace_id" => workspace.fetch("id"), "title" => "Shell containment"
    )
    workspace_path = File.join(local_root, "state", "workspaces", TENANT_ID, workspace.fetch("id"))
    FileUtils.mkdir_p(workspace_path, mode: 0o700)

    create = authorized_request(
      Net::HTTP::Post, "/v1/sessions/#{session.fetch('id')}/runs", token,
      {
        "agent_version_id" => agent_version.fetch("id"),
        "workspace_id" => workspace.fetch("id"),
        "model_policy_id" => model_policy.fetch("id"),
        "input" => "Run the containment probe.",
        "budget" => { "max_tokens" => 4_096, "max_cost_cents" => 100, "max_duration_seconds" => 120 }
      }
    )
    create["Idempotency-Key"] = SecureRandom.uuid
    run_id = parse_json_response(direct_http(api, create), 202).fetch("run_id")

    approval = eventually("shell approval") { pending_approval(api, token, run_id) }
    unless approval["tool_name"] == "shell.exec"
      raise "the approval was raised for #{approval['tool_name'].inspect}, not shell.exec"
    end
    # The reviewer must see the command they are approving.
    unless approval.dig("arguments", "command").to_s.include?("probe_done")
      raise "the approval did not carry the command under review: #{approval['arguments']}"
    end

    decision = authorized_request(
      Net::HTTP::Post, "/v1/approvals/#{approval.fetch('id')}:decide", token,
      "decision" => "allow_once", "version" => approval.fetch("version"),
      "reason" => "shell containment live gate"
    )
    parse_json_response(direct_http(api, decision), 200)

    durable = eventually("terminal Run", timeout: 120) do
      state = psql_json(local_root, ports.fetch(:postgres), <<~SQL)
        select json_build_object(
          'status', r.status,
          'tool_states', (select json_agg(t.state) from tool_executions t
                           where t.tenant_id = r.tenant_id and t.run_id = r.id),
          'tool_sandboxes', (select json_agg(t.sandbox) from tool_executions t
                              where t.tenant_id = r.tenant_id and t.run_id = r.id),
          'stdout', (select e.payload->'content'->>'stdout' from run_events e
                      where e.tenant_id = r.tenant_id and e.run_id = r.id
                        and e.type = 'tool.result' limit 1)
        )::text
          from runs r where r.tenant_id = '#{TENANT_ID}' and r.id = '#{run_id}'
      SQL
      state if %w[succeeded failed cancelled].include?(state&.fetch("status", nil))
    end

    raise "the Run did not succeed: #{durable['status']}" unless durable["status"] == "succeeded"
    unless durable["tool_states"] == ["completed"]
      raise "shell.exec did not execute exactly once: #{durable['tool_states'].inspect}"
    end
    unless durable["tool_sandboxes"] == ["trusted_native"]
      raise "shell.exec ran outside the trusted native sandbox: #{durable['tool_sandboxes'].inspect}"
    end

    probe = parse_probe(durable.fetch("stdout").to_s)
    raise "the probe did not run to completion: #{probe}" unless probe.key?("probe_done") ||
                                                                 durable["stdout"].include?("probe_done")

    # Containment, observed from inside the container in the production path.
    unless probe["key"] == "[unset]"
      raise "the Worker environment reached a model-authored command: #{probe['key']}"
    end
    unless probe["tmp_write"] == "DENIED"
      raise "a command wrote outside its Workspace"
    end
    raise "the escape marker was created" if File.exist?(ESCAPE_MARKER)
    unless probe["workspace_write"] == "ok"
      raise "a command could not write inside its own Workspace, so the checks above " \
            "may be passing because nothing ran: #{probe}"
    end
    unless probe["home"].to_s.end_with?(".agent-home") &&
           probe["home"].to_s.include?(TENANT_ID)
      raise "HOME did not point inside the Workspace: #{probe['home']}"
    end
    unless probe["cwd"].to_s.include?(TENANT_ID)
      raise "the command did not start in the Workspace: #{probe['cwd']}"
    end

    # A denied path that does not exist reports ENOENT, indistinguishable from a
    # path that was never denied, so only directories present on this host can
    # prove the denial. Refuse to report a pass when none were available.
    probed = Integer(probe.fetch("credentials_probed", "0"), 10)
    if probed.zero?
      raise "no credential directory exists on this host, so credential denial was not " \
            "exercised; this gate cannot report a pass here"
    end
    probed.times do |index|
      %w[list read].each do |operation|
        verdict = probe["cred#{index}_#{operation}"]
        unless verdict == "DENIED"
          raise "a contained command could #{operation} a real credential directory " \
                "(cred#{index}_#{operation}=#{verdict})"
        end
      end
    end

    evidence = eventually("provider evidence", timeout: 10) do
      JSON.parse(File.read(provider_evidence)) if File.file?(provider_evidence)
    end
    evidence.fetch("turns").each do |turn|
      unless turn["advertised_tools"] == ["shell.exec"]
        raise "turn #{turn['turn']} advertised #{turn['advertised_tools'].inspect}; the delegated " \
              "scope only covers shell.exec"
      end
      raise "a file Tool leaked into the model catalog" if turn["file_tools_leaked"]
    end

    puts "credential directories exercised: #{probed}"
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
  FileUtils.rm_f(ESCAPE_MARKER)
  open_ports = ports.values.select { |port| port_open?(port) }
  failure ||= RuntimeError.new("shell gate left ports open: #{open_ports}") unless open_ports.empty?
  failure ||= RuntimeError.new("shell gate left local state behind") if File.exist?(local_root)
  if failure
    detail = File.file?(provider_log) ? File.read(provider_log).gsub(provider_secret, "[REDACTED]") : ""
    warn "#{failure.class}: #{failure.message}\n#{detail}"
  end
  FileUtils.remove_entry(temporary) if File.exist?(temporary)
end

raise failure if failure

puts "validated the Shell Tool's containment from inside the container with complete cleanup"
