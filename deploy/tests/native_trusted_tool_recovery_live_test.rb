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
require "uri"
require_relative "support/utf8_diagnostics"

ROOT = File.expand_path("../..", __dir__)
SUPERVISOR = File.join(ROOT, "deploy", "native", "supervisor")
TOOL_PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "openai_tool_provider.rb")
RATE_LIMIT_PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "rate_limited_provider.rb")
DOWNLOAD_WRAPPER = File.join(ROOT, "deploy", "native", "with-download-proxy")
TENANT_ID = "11111111-1111-4111-8111-111111111111"
APPLICATION_ID = "22222222-2222-4222-8222-222222222222"
AGENT_INSTRUCTIONS = "Inspect durable workspace evidence before acting; report only verified results."
SKILL_NAME = "recovery-workspace-review"
SKILL_VERSION = "1.0.0"
SKILL_INSTRUCTIONS = "Read the bounded workspace fixture before answering."
EFFECTIVE_SYSTEM_INSTRUCTIONS = "#{AGENT_INSTRUCTIONS}\n\n" \
  "[Skill #{SKILL_NAME}@#{SKILL_VERSION}]\n#{SKILL_INSTRUCTIONS}"
WORKSPACE_FIXTURE = "Agent Runtime native workspace: trusted read-only Tool fixture."
EXPECTED_EVENTS = %w[
  run.started
  model.usage
  model.tool_call
  model.turn.completed
  approval.required
  run.restored
  approval.rebound
  run.resumed
  tool.execution.started
  tool.result
  model.output.delta
  model.usage
  run.succeeded
].freeze

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

def psql_query(local_root, postgres_port, sql)
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

  output.strip
end

def psql_json(local_root, postgres_port, sql)
  value = psql_query(local_root, postgres_port, sql)
  return nil if value.empty?

  JSON.parse(value.lines.last)
end

def pending_approval(api_port, token, run_id)
  response = direct_http(
    api_port,
    authorized_request(
      Net::HTTP::Get,
      "/v1/approvals?status=pending&limit=50",
      token
    )
  )
  body = parse_json_response(response, 200)
  Array(body["items"]).find { |item| item["run_id"] == run_id }
end

def run_status(api_port, token, run_id)
  response = direct_http(api_port, authorized_request(Net::HTTP::Get, "/v1/runs", token))
  body = parse_json_response(response, 200)
  Array(body["items"]).find { |item| item["id"] == run_id }&.fetch("status")
end

def rss_kib(local_root)
  pgids = Dir.glob(File.join(local_root, "run", "*.pgid")).map do |path|
    Integer(File.read(path).strip, 10)
  rescue ArgumentError
    nil
  end.compact
  postgres_root = File.join(local_root, "state", "postgres")
  output, status = Open3.capture2("ps", "-axo", "pid=,pgid=,rss=,command=")
  raise "failed to inspect native runtime memory" unless status.success?

  output.lines.sum do |line|
    pid, pgid, rss, command = line.strip.split(/\s+/, 4)
    next 0 unless pid && pgid && rss && command
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

def ordered_subsequence?(actual, expected)
  cursor = 0
  actual.each do |item|
    cursor += 1 if item == expected[cursor]
    return true if cursor == expected.length
  end
  false
end

temporary = Dir.mktmpdir("agent-runtime-recovery-live-")
local_root = File.join(temporary, ".local")
provider_ready = File.join(temporary, "provider-ready")
provider_evidence = File.join(temporary, "provider-evidence.json")
provider_stdout = File.join(temporary, "provider.stdout.log")
provider_stderr = File.join(temporary, "provider.stderr.log")
provider_secret = "live-#{SecureRandom.hex(16)}"
rate_limit_provider_ready = File.join(temporary, "rate-limit-provider-ready")
rate_limit_provider_evidence = File.join(temporary, "rate-limit-provider-evidence.json")
rate_limit_provider_stdout = File.join(temporary, "rate-limit-provider.stdout.log")
rate_limit_provider_stderr = File.join(temporary, "rate-limit-provider.stderr.log")
rate_limit_provider_secret = "rate-limit-live-#{SecureRandom.hex(16)}"
ports = {
  provider: free_loopback_port,
  rate_limit_provider: free_loopback_port,
  postgres: free_loopback_port,
  nats: free_loopback_port,
  nats_monitor: free_loopback_port,
  api: free_loopback_port,
  management: free_loopback_port,
  model_health: free_loopback_port,
  model_grpc: free_loopback_port,
  checkpoint_health: free_loopback_port,
  checkpoint_grpc: free_loopback_port,
  worker_health: free_loopback_port,
  console: free_loopback_port
}
provider_pid = nil
rate_limit_provider_pid = nil
runtime_started = false
failure = nil

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
  "AGENT_RUNTIME_PROVIDER_ENDPOINT" => "http://127.0.0.1:#{ports.fetch(:provider)}/v1/chat/completions",
  "AGENT_RUNTIME_PROVIDER_MODEL" => "local-tool-recovery-test",
  "AGENT_RUNTIME_PROVIDER_API_KEY" => provider_secret,
  "AGENT_RUNTIME_SCHEDULER_LEASE_DURATION" => "PT5S",
  "AGENT_RUNTIME_SCHEDULER_HEARTBEAT_FRESHNESS" => "PT3S",
  "AGENT_RUNTIME_STARTUP_ATTEMPTS" => "900"
}

begin
  Timeout.timeout(300) do
    cached_nats = File.join(ROOT, ".local", "toolchain", "nats-server")
    cached_nats_version, cached_nats_status = if File.executable?(cached_nats)
                                                Open3.capture2e(cached_nats, "--version")
                                              else
                                                ["", nil]
                                              end
    if cached_nats_status&.success? && cached_nats_version.include?("v2.10.20")
      FileUtils.mkdir_p(File.join(local_root, "toolchain"))
      FileUtils.cp(cached_nats, File.join(local_root, "toolchain", "nats-server"))
      FileUtils.chmod(0o755, File.join(local_root, "toolchain", "nats-server"))
    end
    provider_pid = Process.spawn(
      {
        "AGENT_RUNTIME_TEST_PROVIDER_PORT" => ports.fetch(:provider).to_s,
        "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => provider_secret,
        "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => provider_ready,
        "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => provider_evidence,
        "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_SYSTEM_INSTRUCTIONS" => EFFECTIVE_SYSTEM_INSTRUCTIONS
      },
      TOOL_PROVIDER,
      chdir: ROOT,
      out: provider_stdout,
      err: provider_stderr,
      pgroup: true
    )
    eventually("loopback model provider", timeout: 10) do
      File.file?(provider_ready) && process_alive?(provider_pid)
    end

    rate_limit_provider_pid = Process.spawn(
      {
        "AGENT_RUNTIME_TEST_PROVIDER_PORT" => ports.fetch(:rate_limit_provider).to_s,
        "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => rate_limit_provider_secret,
        "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => rate_limit_provider_ready,
        "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => rate_limit_provider_evidence,
        "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_MODEL" => "local-rate-limited-test",
        "AGENT_RUNTIME_TEST_PROVIDER_EXPECTED_REQUESTS" => "2"
      },
      RATE_LIMIT_PROVIDER,
      chdir: ROOT,
      out: rate_limit_provider_stdout,
      err: rate_limit_provider_stderr,
      pgroup: true
    )
    eventually("rate-limited loopback model provider", timeout: 10) do
      File.file?(rate_limit_provider_ready) && process_alive?(rate_limit_provider_pid)
    end

    run_command(environment, SUPERVISOR, "start")
    runtime_started = true
    token = File.read(File.join(local_root, "secrets", "local-access-token")).strip

    context = parse_json_response(
      direct_http(
        ports.fetch(:api),
        authorized_request(Net::HTTP::Get, "/v1/console/resource-context", token)
      ),
      200
    )
    unless context["application_id"] == APPLICATION_ID && context["tenant_id"].nil? &&
           context.fetch("projects").length == 1
      raise "resource context crossed its claimed Application boundary: #{context}"
    end
    project_id = context.fetch("projects").first.fetch("id")
    workspace = create_resource(
      ports.fetch(:api), token, "/v1/workspaces",
      "project_id" => project_id, "name" => "Recovery Evidence Workspace"
    )
    agent = create_resource(
      ports.fetch(:api), token, "/v1/agents",
      "workspace_id" => workspace.fetch("id"), "name" => "Recovery Evidence Agent"
    )
    skill_version = create_resource(
      ports.fetch(:api), token, "/v1/skills:publish",
      "name" => SKILL_NAME,
      "semantic_version" => SKILL_VERSION,
      "description" => "Review bounded workspace evidence",
      "instructions" => SKILL_INSTRUCTIONS,
      "tool_names" => ["workspace.read_text"],
      "supported_platforms" => ["darwin-arm64", "linux-arm64", "linux-x86_64"],
      "min_runtime_version" => "0.1.0"
    )
    unless skill_version.fetch("artifact_digest").match?(/\A[0-9a-f]{64}\z/) &&
           skill_version.fetch("signature").length == 86
      raise "Skill publication did not return a signed immutable artifact"
    end
    agent_version = create_resource(
      ports.fetch(:api), token, "/v1/agents/#{agent.fetch('id')}/versions",
      "instructions" => AGENT_INSTRUCTIONS,
      "delegated_scopes" => ["tool:workspace.read"],
      "skill_version_ids" => [skill_version.fetch("id")]
    )
    rate_limited_provider = create_resource(
      ports.fetch(:api), token, "/v1/model-providers",
      "name" => "Rate-limited primary",
      "protocol" => "openai_compatible",
      "endpoint" => "http://127.0.0.1:#{ports.fetch(:rate_limit_provider)}/v1/chat/completions",
      "model" => "local-rate-limited-test",
      "api_key" => rate_limit_provider_secret
    )
    tool_provider = create_resource(
      ports.fetch(:api), token, "/v1/model-providers",
      "name" => "Trusted Tool fallback",
      "protocol" => "openai_compatible",
      "endpoint" => "http://127.0.0.1:#{ports.fetch(:provider)}/v1/chat/completions",
      "model" => "local-tool-recovery-test",
      "api_key" => provider_secret
    )
    model_policy = create_resource(
      ports.fetch(:api), token, "/v1/model-policies",
      "workspace_id" => workspace.fetch("id"),
      "name" => "Recovery Evidence Policy",
      "routing" => "ordered_failover",
      "provider_ids" => [rate_limited_provider.fetch("id"), tool_provider.fetch("id")]
    )
    [rate_limited_provider, tool_provider].each do |provider|
      unless provider["credential_status"] == "configured" &&
             !provider.key?("api_key") && !provider.key?("credential_envelope")
        raise "Provider API exposed credential material: #{provider.keys.sort}"
      end
    end
    provider_registry = psql_json(local_root, ports.fetch(:postgres), <<~SQL)
      select json_build_object(
        'candidate_ids',(
          select json_agg(c.provider_id order by c.priority)
            from model_policy_candidates c
           where c.tenant_id = '#{TENANT_ID}'
             and c.model_policy_id = '#{model_policy.fetch('id')}'
        ),
        'plaintext_matches',(
          select count(*) from model_providers p
           where p.tenant_id = '#{TENANT_ID}'
             and (
               position('#{rate_limit_provider_secret}' in p.credential_envelope::text) > 0
               or position('#{provider_secret}' in p.credential_envelope::text) > 0
             )
        )
      )::text
    SQL
    expected_provider_ids = [rate_limited_provider.fetch("id"), tool_provider.fetch("id")]
    unless provider_registry == {
      "candidate_ids" => expected_provider_ids,
      "plaintext_matches" => 0
    }
      raise "Provider registry order or ciphertext boundary is invalid: #{provider_registry}"
    end
    session = create_resource(
      ports.fetch(:api), token, "/v1/sessions",
      "workspace_id" => workspace.fetch("id"), "title" => "Recovery evidence"
    )
    workspace_path = File.join(
      local_root, "state", "workspaces", TENANT_ID, workspace.fetch("id")
    )
    FileUtils.mkdir_p(workspace_path, mode: 0o700)
    FileUtils.chmod(0o700, [File.dirname(workspace_path), workspace_path])
    File.write(
      File.join(workspace_path, "README.txt"),
      "#{WORKSPACE_FIXTURE}\n",
      mode: "w",
      perm: 0o600
    )

    create = authorized_request(
      Net::HTTP::Post,
      "/v1/sessions/#{session.fetch('id')}/runs",
      token,
      {
        "agent_version_id" => agent_version.fetch("id"),
        "workspace_id" => workspace.fetch("id"),
        "model_policy_id" => model_policy.fetch("id"),
        "input" => "Read the trusted native workspace fixture and report its content.",
        "budget" => {
          "max_tokens" => 4_096,
          "max_cost_cents" => 100,
          "max_duration_seconds" => 120
        }
      }
    )
    create["Idempotency-Key"] = SecureRandom.uuid
    accepted = parse_json_response(direct_http(ports.fetch(:api), create), 202)
    run_id = accepted.fetch("run_id")
    raise "Run acknowledgement contains an invalid events URL" unless accepted["events_url"] == "/v1/runs/#{run_id}/events"

    approval = eventually("persisted Tool approval", timeout: 45) do
      pending_approval(ports.fetch(:api), token, run_id)
    end
    raise "Console approval omitted reviewed arguments" unless approval["arguments"] == { "path" => "README.txt" }
    raise "Console approval omitted trusted native policy" unless approval.values_at("effect", "sandbox") == %w[pure trusted_native]

    initial = eventually("waiting-approval checkpoint", timeout: 30) do
      checkpoint = psql_json(local_root, ports.fetch(:postgres), <<~SQL)
        select json_build_object(
          'run_status',r.status,
          'approval_status',a.status,
          'approval_version',a.version,
          'attempt_id',a.attempt_id,
          'worker_id',a.worker_id,
          'worker_incarnation_id',a.worker_incarnation_id,
          'owner_epoch',d.owner_epoch,
          'checkpoint_status',c.status,
          'checkpoint_sequence',c.sequence
        )::text
          from approvals a
          join runs r on r.tenant_id = a.tenant_id and r.id = a.run_id
          join run_dispatches d
            on d.tenant_id = a.tenant_id and d.run_id = a.run_id and d.attempt_id = a.attempt_id
          join lateral (
            select status,sequence from run_checkpoints c
             where c.tenant_id = a.tenant_id and c.run_id = a.run_id
               and c.attempt_id = a.attempt_id
             order by sequence desc limit 1
          ) c on true
         where a.tenant_id = '#{TENANT_ID}' and a.id = '#{approval.fetch('id')}'
           and r.status = 'waiting_approval' and a.status = 'pending'
      SQL
      expected = ["waiting_approval", "pending", 1, "waiting_approval", 5]
      checkpoint if checkpoint&.values_at(
        "run_status", "approval_status", "approval_version", "checkpoint_status", "checkpoint_sequence"
      ) == expected
    end
    unless initial.values_at("run_status", "approval_status", "approval_version",
                              "checkpoint_status", "checkpoint_sequence") ==
           ["waiting_approval", "pending", 1, "waiting_approval", 5]
      raise "approval was not durably checkpointed before failure: #{initial}"
    end

    worker_pid_path = File.join(local_root, "run", "runtime-worker.pid")
    worker_pgid_path = File.join(local_root, "run", "runtime-worker.pgid")
    old_worker_pid = Integer(File.read(worker_pid_path).strip, 10)
    old_worker_pgid = Integer(File.read(worker_pgid_path).strip, 10)
    worker_command = `ps -p #{old_worker_pid} -o command=`.strip
    actual_pgid = Integer(`ps -p #{old_worker_pid} -o pgid=`.strip, 10)
    current_pgid = Process.getpgrp
    unless process_alive?(old_worker_pid) && worker_command.include?("agent-runtime-worker") &&
           actual_pgid == old_worker_pgid && old_worker_pgid > 1 && old_worker_pgid != current_pgid
      raise "refusing to hard-kill an unverified worker process"
    end
    Process.kill("KILL", -old_worker_pgid)
    eventually("old worker process exit", timeout: 10) { !process_alive?(old_worker_pid) }

    run_command(environment, SUPERVISOR, "restart", "runtime-worker")
    replacement_pid = Integer(File.read(worker_pid_path).strip, 10)
    raise "replacement worker reused the failed process" if replacement_pid == old_worker_pid

    recovered = eventually("approval rebound to replacement worker", timeout: 45) do
      recovery_sql = <<~SQL
        select json_build_object(
          'attempt_id',a.attempt_id,
          'worker_id',a.worker_id,
          'worker_incarnation_id',a.worker_incarnation_id,
          'owner_epoch',d.owner_epoch,
          'rebound_events',(
            select count(*) from run_events e
             where e.tenant_id = a.tenant_id and e.run_id = a.run_id
               and e.type = 'approval.rebound'
          )
        )::text
          from approvals a
          join run_dispatches d
            on d.tenant_id = a.tenant_id and d.run_id = a.run_id and d.attempt_id = a.attempt_id
         where a.tenant_id = '#{TENANT_ID}' and a.id = '#{approval.fetch('id')}'
           and a.status = 'pending' and d.state = 'accepted'
      SQL
      value = psql_json(local_root, ports.fetch(:postgres), recovery_sql)
      value if value && value["rebound_events"] == 1
    end
    raise "recovery did not create a new attempt" if recovered["attempt_id"] == initial["attempt_id"]
    if recovered["worker_incarnation_id"] == initial["worker_incarnation_id"]
      raise "recovery did not create a new worker incarnation"
    end
    unless recovered["worker_id"] == initial["worker_id"] &&
           recovered["owner_epoch"] == initial["owner_epoch"] + 1
      raise "recovery violated stable worker identity or owner epoch fencing: #{recovered}"
    end

    resident_kib = rss_kib(local_root)
    raise "native runtime exceeded 4 GiB RSS: #{resident_kib} KiB" unless resident_kib.positive? && resident_kib < 4 * 1024 * 1024

    browser_output = File.join(temporary, "playwright")
    run_command(
      environment.merge(
        "AGENT_RUNTIME_LIVE_CONSOLE_URL" => "http://127.0.0.1:#{ports.fetch(:console)}",
        "AGENT_RUNTIME_LIVE_BROWSER_OUTPUT_DIR" => browser_output,
        "AGENT_RUNTIME_LIVE_RUN_ID" => run_id,
        "AGENT_RUNTIME_LIVE_APPROVAL_ID" => approval.fetch("id"),
        "AGENT_RUNTIME_LIVE_AGENT_NAME" => agent.fetch("name"),
        "AGENT_RUNTIME_LIVE_WORKSPACE_NAME" => workspace.fetch("name"),
        "AGENT_RUNTIME_LIVE_BEFORE_SCREENSHOT" => File.join(
          ROOT, "docs", "evidence", "2026-08-02-native-console-approval-pending.png"
        ),
        "AGENT_RUNTIME_LIVE_AFTER_SCREENSHOT" => File.join(
          ROOT, "docs", "evidence", "2026-08-02-native-console-run-succeeded.png"
        )
      ),
      DOWNLOAD_WRAPPER,
      # Scope to this scenario's spec. The shared config uses testDir ./e2e-live,
      # so an unscoped run also loads sibling specs whose own required
      # environment this harness never supplies.
      "pnpm", "--filter", "@agent-runtime/console", "exec", "playwright", "test",
      "e2e-live/native-approval.spec.ts", "--config", "playwright.native-live.config.ts"
    )

    eventually("successful recovered Run after browser approval", timeout: 10) do
      run_status(ports.fetch(:api), token, run_id) == "succeeded"
    end

    events_request = authorized_request(Net::HTTP::Get, "/v1/runs/#{run_id}/events", token)
    events_request["Accept"] = "text/event-stream"
    events_response = direct_http(ports.fetch(:api), events_request, timeout: 30)
    raise "SSE replay failed with HTTP #{events_response.code}" unless events_response.code == "200"
    event_types = events_response.body.lines.map do |line|
      line.delete_prefix("event:").strip if line.start_with?("event:")
    end.compact
    unless ordered_subsequence?(event_types, EXPECTED_EVENTS)
      raise "SSE recovery sequence is incomplete: #{event_types.inspect}"
    end

    final = psql_json(local_root, ports.fetch(:postgres), <<~SQL)
      select json_build_object(
        'run_status',r.status,
        'approval_status',a.status,
        'approval_version',a.version,
        'dispatch_states',(
          select json_agg(d.state order by d.requested_at)
            from run_dispatches d
           where d.tenant_id = r.tenant_id and d.run_id = r.id
        ),
        'recovery_state',(
          select i.state from recovery_incidents i
           where i.tenant_id = r.tenant_id and i.run_id = r.id
           order by i.detected_at desc limit 1
        ),
        'completed_tool_count',(
          select count(*) from tool_executions t
           where t.tenant_id = r.tenant_id and t.run_id = r.id and t.state = 'completed'
        ),
        'trusted_native_tool_count',(
          select count(*) from tool_executions t
           where t.tenant_id = r.tenant_id and t.run_id = r.id
             and t.state = 'completed' and t.sandbox = 'trusted_native'
        )
      )::text
        from runs r
        join approvals a on a.tenant_id = r.tenant_id and a.run_id = r.id
       where r.tenant_id = '#{TENANT_ID}' and r.id = '#{run_id}'
    SQL
    unless final["run_status"] == "succeeded" && final["approval_status"] == "approved" &&
           final["approval_version"] == 2 && final["dispatch_states"] == %w[lost finished] &&
           final["recovery_state"] == "recovered" && final["completed_tool_count"] == 1 &&
           final["trusted_native_tool_count"] == 1
      raise "final durable recovery evidence is inconsistent: #{final}"
    end

    provider_result = eventually("provider evidence", timeout: 10) do
      JSON.parse(File.read(provider_evidence)) if File.file?(provider_evidence)
    end
    unless provider_result == {
      "requests" => 2,
      "tool" => "workspace.read_text",
      "path" => "README.txt",
      "result_verified" => true,
      "system_instructions_verified" => true
    }
      raise "provider did not verify the recovered Tool result: #{provider_result}"
    end
    rate_limit_result = eventually("rate-limit Provider evidence", timeout: 10) do
      JSON.parse(File.read(rate_limit_provider_evidence)) if File.file?(rate_limit_provider_evidence)
    end
    unless rate_limit_result == {
      "requests" => 2,
      "authorization_verified" => true,
      "model_verified" => true,
      "status" => 429
    }
      raise "primary Provider did not prove safe failover on both turns: #{rate_limit_result}"
    end

    worker_log = File.read(File.join(local_root, "logs", "runtime-worker.stdout.log"))
    unless worker_log.include?("workload identity renewed")
      raise "live recovery did not exercise workload identity renewal"
    end
    if worker_log.include?("workload identity renewal does not match the active execution")
      raise "live recovery observed a workload identity timestamp or fencing mismatch"
    end

    puts JSON.generate(
      "run_id" => run_id,
      "workspace_id" => workspace.fetch("id"),
      "agent_version_id" => agent_version.fetch("id"),
      "skill_version_id" => skill_version.fetch("id"),
      "approval_id" => approval.fetch("id"),
      "old_attempt_id" => initial.fetch("attempt_id"),
      "new_attempt_id" => recovered.fetch("attempt_id"),
      "old_owner_epoch" => initial.fetch("owner_epoch"),
      "new_owner_epoch" => recovered.fetch("owner_epoch"),
      "rss_kib" => resident_kib,
      "provider_ids" => expected_provider_ids,
      "safe_failovers" => rate_limit_result.fetch("requests"),
      "browser_approval" => "passed",
      "events" => event_types
    )
  end
rescue StandardError, Timeout::Error => error
  failure = error
ensure
  failure_logs = Dir.glob(File.join(local_root, "logs", "*.log")).sort.map do |path|
    content = Utf8Diagnostics.normalize(File.binread(path))
    "--- #{File.basename(path)} ---\n#{content.lines.last(80).join}"
  end.join
  if provider_pid && process_alive?(provider_pid)
    Process.kill("TERM", -provider_pid) rescue nil
    eventually("provider shutdown", timeout: 5, interval: 0.1) { !process_alive?(provider_pid) } rescue nil
  end
  Process.wait(provider_pid) rescue nil if provider_pid
  if rate_limit_provider_pid && process_alive?(rate_limit_provider_pid)
    Process.kill("TERM", -rate_limit_provider_pid) rescue nil
    eventually("rate-limit provider shutdown", timeout: 5, interval: 0.1) do
      !process_alive?(rate_limit_provider_pid)
    end rescue nil
  end
  Process.wait(rate_limit_provider_pid) rescue nil if rate_limit_provider_pid

  begin
    if runtime_started || File.exist?(File.join(local_root, ".agent-runtime-local-root"))
      run_command(environment, SUPERVISOR, "clean")
    end
  rescue StandardError => cleanup_error
    failure ||= cleanup_error
  end

  open_ports = ports.values.select { |port| port_open?(port) }
  failure ||= RuntimeError.new("native recovery test left listening ports: #{open_ports.inspect}") unless open_ports.empty?
  failure ||= RuntimeError.new("native recovery test left local state at #{local_root}") if File.exist?(local_root)

  diagnostic = if failure
                 Utf8Diagnostics.join(
                   failure.full_message,
                   failure_logs,
                   File.binread(provider_stderr),
                   File.binread(rate_limit_provider_stderr)
                 ).gsub(provider_secret, "[REDACTED]")
                   .gsub(rate_limit_provider_secret, "[REDACTED]")
               end
  FileUtils.rm_rf(temporary) if temporary.start_with?(Dir.tmpdir + File::SEPARATOR)
  raise diagnostic if failure
end

puts "validated native trusted Tool checkpoint recovery after hard-killed Worker"
