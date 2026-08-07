#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "securerandom"
require "socket"
require "timeout"
require "tmpdir"
require_relative "support/utf8_diagnostics"

ROOT = File.expand_path("../..", __dir__)
SUPERVISOR = File.join(ROOT, "deploy", "native", "supervisor")
PROVIDER = File.join(ROOT, "deploy", "tests", "fixtures", "openai_single_turn_provider.rb")
INPUT = "Prove the one-command native Agent path."
FINAL_TEXT = "native one-command path verified"

raise "single-turn Provider fixture is missing" unless File.executable?(PROVIDER)

def free_loopback_port
  server = TCPServer.new("127.0.0.1", 0)
  server.addr[1]
ensure
  server&.close
end

def eventually(label, timeout: 20, interval: 0.1)
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

def port_open?(port)
  socket = TCPSocket.new("127.0.0.1", port)
  socket.close
  true
rescue Errno::ECONNREFUSED, Errno::EHOSTUNREACH
  false
end

temporary = Dir.mktmpdir("agent-runtime-one-command-live-")
local_root = File.join(temporary, ".local")
provider_ready = File.join(temporary, "provider-ready")
provider_evidence = File.join(temporary, "provider-evidence.json")
provider_log = File.join(temporary, "provider.log")
provider_secret = "one-command-#{SecureRandom.hex(16)}"
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
  "AGENT_RUNTIME_PROVIDER_MODEL" => "local-one-command-test",
  "AGENT_RUNTIME_PROVIDER_API_KEY" => provider_secret,
  "AGENT_RUNTIME_DOWNLOAD_PROXY" => "http://127.0.0.1:10808",
  "AGENT_RUNTIME_RUN_INPUT" => INPUT,
  "AGENT_RUNTIME_STARTUP_ATTEMPTS" => "900"
}
provider_pid = nil
failure = nil

begin
  Timeout.timeout(300) do
    provider_pid = Process.spawn(
      {
        "AGENT_RUNTIME_TEST_PROVIDER_PORT" => ports.fetch(:provider).to_s,
        "AGENT_RUNTIME_TEST_PROVIDER_API_KEY" => provider_secret,
        "AGENT_RUNTIME_TEST_PROVIDER_READY_FILE" => provider_ready,
        "AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE" => provider_evidence,
        "AGENT_RUNTIME_TEST_PROVIDER_INPUT" => INPUT,
        "AGENT_RUNTIME_TEST_PROVIDER_FINAL_TEXT" => FINAL_TEXT
      },
      PROVIDER, chdir: ROOT, out: provider_log, err: provider_log, pgroup: true
    )
    eventually("single-turn Provider") do
      File.file?(provider_ready) && process_alive?(provider_pid)
    end

    output, error, status = Open3.capture3(environment, "make", "dev-run", chdir: ROOT)
    combined = Utf8Diagnostics.join(output, error, separator: "")
    raise "one-command native Run failed (#{status.exitstatus}):\n#{combined}" unless status.success?
    raise "one-command Run did not expose its accepted id" unless output.include?("Run accepted:")
    raise "one-command Run did not stream the Provider result" unless output.include?(FINAL_TEXT)
    raise "one-command Run did not reach success" unless output.include?("run.succeeded")
    raise "Provider secret leaked to command output" if combined.include?(provider_secret)

    evidence = eventually("single-turn Provider evidence") do
      JSON.parse(File.read(provider_evidence)) if File.file?(provider_evidence)
    end
    unless evidence == { "requests" => 1, "input_count" => 1, "streamed" => true }
      raise "single-turn Provider evidence is incomplete: #{evidence}"
    end
  end
rescue StandardError, Timeout::Error => error
  failure = error
ensure
  if File.exist?(File.join(local_root, ".agent-runtime-local-root"))
    _output, error, status = Open3.capture3(environment, SUPERVISOR, "clean", chdir: ROOT)
    failure ||= RuntimeError.new("native clean failed: #{error}") unless status.success?
  end
  if provider_pid && process_alive?(provider_pid)
    Process.kill("TERM", -provider_pid) rescue nil
  end
  Process.wait(provider_pid) rescue nil if provider_pid
  open_ports = ports.values.select { |port| port_open?(port) }
  failure ||= RuntimeError.new("one-command test left ports open: #{open_ports}") unless open_ports.empty?
  failure ||= RuntimeError.new("one-command test left local state behind") if File.exist?(local_root)
  if failure
    detail = File.file?(provider_log) ? File.read(provider_log).gsub(provider_secret, "[REDACTED]") : ""
    warn "#{failure.class}: #{failure.message}\n#{detail}"
  end
  FileUtils.remove_entry(temporary) if File.exist?(temporary)
end

raise failure if failure

puts "validated make dev-run as a real one-command native Agent path with complete cleanup"
