#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
DEVCTL = File.join(ROOT, "deploy", "native", "devctl")

def executable(path, body)
  File.write(path, "#!/bin/sh\nset -eu\n#{body}")
  FileUtils.chmod(0o755, path)
end

def run_devctl(environment, *arguments)
  Open3.capture3(environment, DEVCTL, *arguments, chdir: ROOT)
end

Dir.mktmpdir("agent-runtime-native-test-") do |temporary|
  local_root = File.join(temporary, ".local")
  fake_bin = File.join(temporary, "bin")
  postgres_bin = File.join(temporary, "postgres", "bin")
  FileUtils.mkdir_p([fake_bin, postgres_bin])

  executable(File.join(postgres_bin, "initdb"), <<~'SH')
    data=""
    while [ "$#" -gt 0 ]; do
      [ "$1" = "-D" ] && data="$2" && shift 2 && continue
      shift
    done
    mkdir -p "$data"
    printf '16\n' > "$data/PG_VERSION"
  SH
  executable(File.join(postgres_bin, "pg_ctl"), <<~'SH')
    data=""
    action=""
    tcp_only="false"
    while [ "$#" -gt 0 ]; do
      [ "$1" = "-D" ] && data="$2" && shift 2 && continue
      case "$1" in
        *postgres-socket*) exit 88 ;;
        *"unix_socket_directories=''"*) tcp_only="true" ;;
      esac
      case "$1" in start|stop|status) action="$1" ;; esac
      shift
    done
    case "$action" in
      start) [ "$tcp_only" = "true" ] || exit 89; printf '%s\n' "$$" > "$data/postmaster.pid" ;;
      stop) rm -f "$data/postmaster.pid" ;;
      status) test -f "$data/postmaster.pid" ;;
      *) exit 2 ;;
    esac
  SH
  executable(File.join(postgres_bin, "createdb"), "exit 0\n")
  missing_password_marker = File.join(temporary, "psql-missing-password")
  executable(File.join(postgres_bin, "psql"), <<~SH)
    if [ -z "${PGPASSWORD:-}" ]; then
      : > '#{missing_password_marker}'
      exit 42
    fi
    printf '1\n'
  SH
  executable(File.join(fake_bin, "nats-server"), <<~'SH')
    pid_file=""
    while [ "$#" -gt 0 ]; do
      [ "$1" = "-t" ] && exit 0
      [ "$1" = "-P" ] && pid_file="$2" && shift 2 && continue
      shift
    done
    [ -z "$pid_file" ] || printf '%s\n' "$$" > "$pid_file"
    trap '[ -z "$pid_file" ] || rm -f "$pid_file"; exit 0' TERM INT
    while :; do sleep 1; done
  SH
  launchctl_calls = File.join(temporary, "launchctl-calls")
  executable(File.join(fake_bin, "launchctl"), <<~'SH')
    printf '%s\n' "$1" >> "$FAKE_LAUNCHCTL_CALLS"
    case "$1" in
      submit)
        shift
        program=""
        stdout_path=""
        stderr_path=""
        while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
          case "$1" in
            -p) program="$2"; shift 2 ;;
            -o) stdout_path="$2"; shift 2 ;;
            -e) stderr_path="$2"; shift 2 ;;
            -l) shift 2 ;;
            *) shift ;;
          esac
        done
        [ -n "$program" ] || exit 93
        [ "$stdout_path" != "$stderr_path" ] || exit 94
        [ "$#" -gt 0 ] && shift
        [ "$#" -gt 0 ] && shift
        "$program" "$@" >/dev/null 2>&1 &
        ;;
      remove)
        if [ -s "$FAKE_NATS_PID_FILE" ]; then
          kill "$(cat "$FAKE_NATS_PID_FILE")" 2>/dev/null || true
        fi
        ;;
      *) exit 92 ;;
    esac
  SH
  docker_marker = File.join(temporary, "docker-called")
  executable(File.join(fake_bin, "docker"), "printf called > '#{docker_marker}'\nexit 97\n")

  environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_POSTGRES_BIN_DIR" => postgres_bin,
    "AGENT_RUNTIME_NATS_SERVER_BIN" => File.join(fake_bin, "nats-server"),
    "AGENT_RUNTIME_LOCAL_MODEL_GRPC_PORT" => "25051",
    "AGENT_RUNTIME_LOCAL_CHECKPOINT_GRPC_PORT" => "25052",
    "AGENT_RUNTIME_PRESERVE_BUILD_OUTPUTS" => "true",
    "FAKE_LAUNCHCTL_CALLS" => launchctl_calls,
    "FAKE_NATS_PID_FILE" => File.join(local_root, "run", "nats.pid"),
    "PATH" => "#{fake_bin}:#{ENV.fetch('PATH')}"
  }

  output, error, status = run_devctl(environment, "start-infra")
  raise "start-infra failed: #{output}#{error}" unless status.success?
  raise "local root marker missing" unless File.file?(File.join(local_root, ".agent-runtime-local-root"))
  raise "PostgreSQL was not initialized" unless File.file?(File.join(local_root, "state", "postgres", "PG_VERSION"))
  raise "NATS PID was not recorded" unless File.file?(File.join(local_root, "run", "nats.pid"))
  raise "NATS process group was not recorded" unless File.file?(File.join(local_root, "run", "nats.pgid"))
  raise "NATS config was not generated" unless File.file?(File.join(local_root, "config", "nats.conf"))
  nats_config = File.read(File.join(local_root, "config", "nats.conf"))
  raise "NATS memory store is unbounded" unless nats_config.include?("max_memory_store: 256MB")
  raise "NATS file store is unbounded" unless nats_config.include?("max_file_store: 1GB")
  identity_root = File.join(local_root, "secrets", "identity")
  raise "development identity was not prepared" unless File.file?(
    File.join(identity_root, "identity.ready")
  )
  raise "NATS TLS was not configured" unless nats_config.include?(
    "cert_file: \"#{File.join(identity_root, 'nats-server.crt')}\""
  )
  control_password = File.read(File.join(identity_root, "nats-control-plane-password")).strip
  worker_password = File.read(File.join(identity_root, "nats-worker-password")).strip
  raise "control-plane NATS password leaked into config" if nats_config.include?(control_password)
  raise "worker NATS password leaked into config" if nats_config.include?(worker_password)
  raise "NATS passwords are not bcrypt hashes" unless nats_config.scan(/\$2[aby]\$/).length == 2
  raise "native environment was not generated" unless File.file?(File.join(local_root, "env", "native.env"))
  native_environment = File.read(File.join(local_root, "env", "native.env"))
  raise "native NATS URL is not TLS" unless native_environment.include?(
    "export AGENT_RUNTIME_LOCAL_NATS_URL='tls://127.0.0.1:4222'"
  )
  raise "workload public key was not exported" unless native_environment.include?(
    "export AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY='"
  )
  raise "model gateway endpoint ignored its native port" unless native_environment.include?(
    "export AGENT_RUNTIME_MODEL_GATEWAY_ENDPOINT='https://127.0.0.1:25051'"
  )
  raise "checkpoint gateway endpoint ignored its native port" unless native_environment.include?(
    "export AGENT_RUNTIME_CHECKPOINT_GATEWAY_ENDPOINT='https://127.0.0.1:25052'"
  )
  expected_pid_file = File.join(local_root, "run", "nats.pid")
  raise "NATS PID file was not exported" unless native_environment.include?(
    "export AGENT_RUNTIME_NATS_PID_FILE='#{expected_pid_file}'"
  )
  workspace_root = File.join(local_root, "state", "workspaces")
  seeded_workspace = File.join(
    workspace_root,
    "11111111-1111-4111-8111-111111111111",
    "44444444-4444-4444-8444-444444444444"
  )
  raise "trusted native workspace was not prepared" unless File.directory?(seeded_workspace)
  raise "trusted native workspace fixture is missing" unless File.read(
    File.join(seeded_workspace, "README.txt")
  ).include?("Agent Runtime native workspace")
  raise "trusted native tools were not explicitly enabled" unless native_environment.include?(
    "export AGENT_RUNTIME_TRUSTED_NATIVE_TOOLS='true'"
  )
  raise "trusted native workspace root was not exported" unless native_environment.include?(
    "export AGENT_RUNTIME_WORKSPACE_ROOT='#{workspace_root}'"
  )
  raise "trusted tool binary was not exported" unless native_environment.include?(
    "export AGENT_RUNTIME_TRUSTED_WORKSPACE_TOOL_BIN='#{File.join(ROOT, 'runtime', 'target', 'debug', 'agent-trusted-workspace-tool')}'"
  )
  raise "PostgreSQL probe omitted its password" if File.exist?(missing_password_marker)
  raise "Docker must never be invoked" if File.exist?(docker_marker)

  output, error, status = run_devctl(environment, "status")
  raise "status failed: #{output}#{error}" unless status.success?
  raise "status must report PostgreSQL" unless output.include?("postgresql: running")
  raise "status must report NATS" unless output.include?("nats: running")

  output, error, status = run_devctl(environment, "stop")
  raise "stop failed: #{output}#{error}" unless status.success?
  raise "NATS PID survived stop" if File.exist?(File.join(local_root, "run", "nats.pid"))
  raise "NATS process group survived stop" if File.exist?(File.join(local_root, "run", "nats.pgid"))
  raise "NATS lifecycle unexpectedly called launchd" if File.exist?(launchctl_calls)

  output, error, status = run_devctl(environment, "clean")
  raise "clean failed: #{output}#{error}" unless status.success?
  raise "local state survived clean" if File.exist?(local_root)
  raise "Docker must never be invoked" if File.exist?(docker_marker)
end

puts "validated native development lifecycle"
