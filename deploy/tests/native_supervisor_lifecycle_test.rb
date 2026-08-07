#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
SUPERVISOR = File.join(ROOT, "deploy", "native", "supervisor")

def executable(path, body)
  File.write(path, "#!/bin/sh\nset -eu\n#{body}")
  FileUtils.chmod(0o755, path)
end

def run_supervisor(environment, *arguments)
  Open3.capture3(environment, SUPERVISOR, *arguments, chdir: ROOT)
end

Dir.mktmpdir("agent-runtime-supervisor-test-") do |temporary|
  local_root = File.join(temporary, ".local")
  fake_bin = File.join(temporary, "bin")
  FileUtils.mkdir_p(fake_bin)
  calls = File.join(temporary, "calls")

  executable(File.join(fake_bin, "devctl"), <<~'SH')
    printf 'devctl %s\n' "$*" >> "$FAKE_CALLS"
    case "${1:-}" in
      start-infra)
        [ -f "$FAKE_LOCAL_ROOT/toolchain/nats-server" ] || exit 91
        mkdir -p "$FAKE_LOCAL_ROOT/env" "$FAKE_LOCAL_ROOT/run" \
          "$FAKE_LOCAL_ROOT/logs" "$FAKE_LOCAL_ROOT/secrets"
        : > "$FAKE_LOCAL_ROOT/.agent-runtime-local-root"
        cat > "$FAKE_LOCAL_ROOT/env/native.env" <<'ENVIRONMENT'
    export AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY='test-public-key'
    export AGENT_RUNTIME_WORKLOAD_IDENTITY_PRIVATE_KEY_PKCS8='test-private-key'
    export AGENT_RUNTIME_LOCAL_MODEL_GATEWAY_CERT='/tmp/model.crt'
    export AGENT_RUNTIME_LOCAL_MODEL_GATEWAY_KEY='/tmp/model.key'
    export AGENT_RUNTIME_LOCAL_CHECKPOINT_GATEWAY_CERT='/tmp/checkpoint.crt'
    export AGENT_RUNTIME_LOCAL_CHECKPOINT_GATEWAY_KEY='/tmp/checkpoint.key'
    export AGENT_RUNTIME_LOCAL_GRPC_CA_CERT='/tmp/ca.crt'
    export AGENT_RUNTIME_GRPC_CLIENT_CERT='/tmp/worker.crt'
    export AGENT_RUNTIME_GRPC_CLIENT_KEY='/tmp/worker.key'
    export AGENT_RUNTIME_GRPC_SERVER_CA_CERT='/tmp/ca.crt'
    export AGENT_RUNTIME_MODEL_GATEWAY_ENDPOINT='https://127.0.0.1:50051'
    export AGENT_RUNTIME_MODEL_GATEWAY_TLS_DOMAIN='model-gateway.local'
    export AGENT_RUNTIME_CHECKPOINT_GATEWAY_ENDPOINT='https://127.0.0.1:50052'
    export AGENT_RUNTIME_CHECKPOINT_GATEWAY_TLS_DOMAIN='checkpoint-gateway.local'
    export AGENT_RUNTIME_CHECKPOINT_LOCAL_DIR='/tmp/checkpoints'
    export AGENT_RUNTIME_NATS_URL='tls://127.0.0.1:4222'
    export AGENT_RUNTIME_NATS_USERNAME='runtime-worker'
    export AGENT_RUNTIME_NATS_PASSWORD='worker-password'
    export AGENT_RUNTIME_NATS_CA_CERT='/tmp/ca.crt'
    export AGENT_RUNTIME_OUTBOX_NATS_URL='tls://127.0.0.1:4222'
    export AGENT_RUNTIME_OUTBOX_NATS_SECURITY_TLS_REQUIRED='true'
    export AGENT_RUNTIME_OUTBOX_NATS_SECURITY_USERNAME='control-plane'
    export AGENT_RUNTIME_OUTBOX_NATS_SECURITY_PASSWORD='control-password'
    export AGENT_RUNTIME_OUTBOX_NATS_SECURITY_TRUSTSTORE_PATH='/tmp/control.p12'
    export AGENT_RUNTIME_OUTBOX_NATS_SECURITY_TRUSTSTORE_PASSWORD='truststore-password'
    export AGENT_RUNTIME_SCHEDULER_NATS_URL='tls://127.0.0.1:4222'
    export AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_TLS_REQUIRED='true'
    export AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_USERNAME='control-plane'
    export AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_PASSWORD='control-password'
    export AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_TRUSTSTORE_PATH='/tmp/control.p12'
    export AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_TRUSTSTORE_PASSWORD='truststore-password'
    export SPRING_SECURITY_OAUTH2_RESOURCESERVER_JWT_PUBLIC_KEY_LOCATION='file:/tmp/jwt.pem'
    export AGENT_RUNTIME_WORKER_ID_FILE='/tmp/worker-id'
    export SPRING_DATASOURCE_URL='jdbc:postgresql://127.0.0.1:54329/agent_runtime'
    export SPRING_DATASOURCE_USERNAME='agent_runtime_owner'
    export SPRING_DATASOURCE_PASSWORD='postgres-password'
    export MANAGEMENT_SCRAPE_PASSWORD='metrics-password'
    ENVIRONMENT
        : > "$FAKE_LOCAL_ROOT/run/infra-running"
        ;;
      bootstrap)
        mkdir -p "$FAKE_LOCAL_ROOT/toolchain"
        : > "$FAKE_LOCAL_ROOT/toolchain/nats-server"
        ;;
      status)
        if [ -f "$FAKE_LOCAL_ROOT/run/infra-running" ]; then
          printf 'postgresql: running\nnats: running\n'
        else
          printf 'postgresql: stopped\nnats: stopped\n'
        fi
        ;;
      stop) rm -f "$FAKE_LOCAL_ROOT/run/infra-running" ;;
      clean) rm -rf "$FAKE_LOCAL_ROOT" ;;
      *) exit 2 ;;
    esac
  SH

  executable(File.join(fake_bin, "launchctl"), <<~'SH')
    printf 'launchctl %s\n' "$*" >> "$FAKE_CALLS"
    exit 92
  SH
  executable(File.join(fake_bin, "curl"), <<~'SH')
    printf 'curl %s\n' "$*" >> "$FAKE_CALLS"
    exit 0
  SH
  executable(File.join(fake_bin, "seeder"), <<~'SH')
    printf 'seeder\n' >> "$FAKE_CALLS"
  SH
  executable(File.join(fake_bin, "service-process"), <<~'SH')
    trap 'exit 0' TERM INT
    while :; do sleep 1; done
  SH
  java_home = File.join(temporary, "jdk-21")
  FileUtils.mkdir_p(File.join(java_home, "bin"))
  executable(File.join(java_home, "bin", "java"), <<~'SH')
    if [ "${1:-}" = "-version" ]; then
      printf 'openjdk version "21.0.1"\n' >&2
      exit 0
    fi
    [ "${SERVER_PORT:-}" = "18080" ] || exit 95
    exec "$(dirname "$0")/../../bin/service-process"
  SH
  executable(File.join(fake_bin, "download-wrapper"), <<~'SH')
    printf 'download-wrapper %s\n' "$*" >> "$FAKE_CALLS"
    export HTTP_PROXY='http://127.0.0.1:10808'
    export HTTPS_PROXY='http://127.0.0.1:10808'
    export NO_PROXY='localhost,127.0.0.1,::1,.local'
    exec "$@"
  SH
  %w[agent-model-gateway agent-checkpoint-gateway agent-runtime-worker agent-trusted-workspace-tool].each do |name|
    executable(File.join(fake_bin, name), <<~'SH')
      exec "$(dirname "$0")/service-process"
    SH
  end
  executable(File.join(fake_bin, "agent-model-gateway"), <<~'SH')
    [ "${AGENT_RUNTIME_PROVIDER_PROTOCOL:-}" = "anthropic_messages" ] || exit 94
    [ "${AGENT_RUNTIME_PROVIDER_ANTHROPIC_VERSION:-}" = "2023-06-01" ] || exit 95
    exec "$(dirname "$0")/service-process"
  SH
  executable(File.join(fake_bin, "pnpm"), <<~'SH')
    [ "${AGENT_RUNTIME_CONTROL_API:-}" = "http://127.0.0.1:18080" ] || exit 96
    exec "$(dirname "$0")/service-process"
  SH
  jar = File.join(temporary, "control-plane.jar")
  File.write(jar, "test")
  docker_marker = File.join(temporary, "docker-called")
  executable(File.join(fake_bin, "docker"), "printf called > '#{docker_marker}'\nexit 97\n")

  environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_DEVCTL" => File.join(fake_bin, "devctl"),
    "AGENT_RUNTIME_CURL_BIN" => File.join(fake_bin, "curl"),
    "AGENT_RUNTIME_SEEDER" => File.join(fake_bin, "seeder"),
    "AGENT_RUNTIME_SKIP_BUILD" => "true",
    "AGENT_RUNTIME_DOWNLOAD_WRAPPER" => File.join(fake_bin, "download-wrapper"),
    "JAVA_HOME" => java_home,
    "AGENT_RUNTIME_CONTROL_PLANE_JAR" => jar,
    "AGENT_RUNTIME_MODEL_GATEWAY_BIN" => File.join(fake_bin, "agent-model-gateway"),
    "AGENT_RUNTIME_CHECKPOINT_GATEWAY_BIN" => File.join(fake_bin, "agent-checkpoint-gateway"),
    "AGENT_RUNTIME_WORKER_BIN" => File.join(fake_bin, "agent-runtime-worker"),
    "AGENT_RUNTIME_TRUSTED_WORKSPACE_TOOL_BIN" => File.join(fake_bin, "agent-trusted-workspace-tool"),
    "AGENT_RUNTIME_PNPM_BIN" => File.join(fake_bin, "pnpm"),
    "AGENT_RUNTIME_PROVIDER_ENDPOINT" => "https://provider.example/v1/chat/completions",
    "AGENT_RUNTIME_PROVIDER_MODEL" => "test-model",
    "AGENT_RUNTIME_PROVIDER_API_KEY" => "provider-secret",
    "AGENT_RUNTIME_PROVIDER_PROTOCOL" => "anthropic_messages",
    "AGENT_RUNTIME_PROVIDER_ANTHROPIC_VERSION" => "2023-06-01",
    "FAKE_CALLS" => calls,
    "FAKE_LOCAL_ROOT" => local_root,
    "PATH" => "#{fake_bin}:#{ENV.fetch('PATH')}"
  }

  output, error, status = run_supervisor(environment, "start")
  unless status.success?
    call_log = File.exist?(calls) ? File.read(calls) : "(no lifecycle calls recorded)\n"
    service_logs = Dir.glob(File.join(local_root, "logs", "*.log")).sort.map do |path|
      "--- #{File.basename(path)} ---\n#{File.read(path)}"
    end.join
    raise "supervisor start failed (#{status.exitstatus}): #{output}#{error}#{call_log}#{service_logs}"
  end
  raise "provider secret leaked to output" if (output + error).include?("provider-secret")
  lifecycle_calls = File.readlines(calls).grep(/^devctl /)
  unless lifecycle_calls.take(2) == ["devctl bootstrap\n", "devctl start-infra\n"]
    raise "native tool bootstrap must precede infrastructure startup: #{lifecycle_calls.inspect}"
  end

  output, error, status = run_supervisor(environment, "status")
  raise "supervisor status failed: #{output}#{error}" unless status.success?
  %w[postgresql nats control-plane model-gateway checkpoint-gateway runtime-worker console].each do |service|
    raise "#{service} was not reported running: #{output}#{error}" unless output.include?("#{service}: running")
  end

  provider_secret = File.join(local_root, "secrets", "provider-api-key")
  raise "provider secret was not stored" unless File.read(provider_secret) == "provider-secret"
  provider_protocol = File.join(local_root, "secrets", "provider-protocol")
  raise "provider protocol was not stored" unless File.read(provider_protocol) == "anthropic_messages"
  raise "provider secret permissions are too broad" unless File.stat(provider_secret).mode & 0o077 == 0
  application_launchctl_calls = File.readlines(calls).grep(/^launchctl (?:submit|bootstrap) /)
  unless application_launchctl_calls.empty?
    raise "application lifecycle must not depend on launchd in a Documents workspace"
  end
  service_pgids = Dir.glob(File.join(local_root, "run", "*.pgid"))
  raise "expected five detached process groups, got #{service_pgids.length}" unless service_pgids.length == 5
  health_probes = File.readlines(calls).count { |line| line.start_with?("curl ") }
  raise "all five application health endpoints were not probed" unless health_probes >= 5
  seeded = File.readlines(calls).count { |line| line == "seeder\n" }
  raise "development resources were not seeded exactly once" unless seeded == 1
  raise "Docker must never be invoked" if File.exist?(docker_marker)

  original_worker_pid = Integer(File.read(File.join(local_root, "run", "runtime-worker.pid")), 10)
  output, error, status = run_supervisor(environment, "restart", "runtime-worker")
  raise "worker restart failed: #{output}#{error}" unless status.success?
  replacement_worker_pid = Integer(File.read(File.join(local_root, "run", "runtime-worker.pid")), 10)
  raise "worker restart reused the old process" if replacement_worker_pid == original_worker_pid
  raise "replacement worker is not alive" unless Process.kill(0, replacement_worker_pid) == 1
  %w[control-plane model-gateway checkpoint-gateway console].each do |service|
    pid = Integer(File.read(File.join(local_root, "run", "#{service}.pid")), 10)
    raise "#{service} was disrupted by worker restart" unless Process.kill(0, pid) == 1
  end
  restarted_health_probes = File.readlines(calls).count { |line| line.start_with?("curl ") }
  raise "replacement worker health was not probed" unless restarted_health_probes > health_probes

  output, error, status = run_supervisor(environment, "stop")
  raise "supervisor stop failed: #{output}#{error}" unless status.success?
  Dir.glob(File.join(local_root, "run", "*.pid")).each do |pid_file|
    raise "application PID survived stop: #{pid_file}" if File.exist?(pid_file)
  end
  Dir.glob(File.join(local_root, "run", "*.pgid")).each do |pgid_file|
    raise "application process group survived stop: #{pgid_file}" if File.exist?(pgid_file)
  end

  output, error, status = run_supervisor(environment, "clean")
  raise "supervisor clean failed: #{output}#{error}" unless status.success?
  raise "local state survived clean" if File.exist?(local_root)
  output, error, status = run_supervisor(environment, "clean")
  raise "repeated supervisor clean failed: #{output}#{error}" unless status.success?
  raise "Docker must never be invoked" if File.exist?(docker_marker)
end

puts "validated complete native application supervisor lifecycle"
