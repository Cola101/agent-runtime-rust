#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
VERIFY = File.join(ROOT, "deploy", "tests", "verify_nats_tls.sh")

def executable(path, body)
  File.write(path, "#!/bin/sh\nset -eu\n#{body}")
  FileUtils.chmod(0o755, path)
end

Dir.mktmpdir("agent-runtime-native-nats-live-") do |temporary|
  local_root = File.join(temporary, ".local")
  fake_bin = File.join(temporary, "bin")
  FileUtils.mkdir_p([File.join(local_root, "env"), File.join(local_root, "logs"), fake_bin])

  ca = File.join(local_root, "ca.crt")
  truststore = File.join(local_root, "truststore.p12")
  File.write(ca, "test-ca")
  File.write(truststore, "test-truststore")
  File.write(File.join(local_root, "env", "native.env"), <<~ENVIRONMENT)
    export AGENT_RUNTIME_LOCAL_NATS_URL='tls://127.0.0.1:14222'
    export AGENT_RUNTIME_NATS_USERNAME='runtime-worker'
    export AGENT_RUNTIME_NATS_PASSWORD='worker-secret'
    export AGENT_RUNTIME_NATS_CA_CERT='#{ca}'
    export AGENT_RUNTIME_LOCAL_NATS_CONTROL_USERNAME='control-plane'
    export AGENT_RUNTIME_LOCAL_NATS_CONTROL_PASSWORD='control-secret'
    export AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE='#{truststore}'
    export AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE_PASSWORD='truststore-secret'
  ENVIRONMENT

  devctl_calls = File.join(temporary, "devctl-calls")
  executable(File.join(fake_bin, "devctl"), <<~SH)
    printf '%s\n' "$*" >> '#{devctl_calls}'
    printf 'postgresql: running\nnats: running\n'
  SH
  cargo_calls = File.join(temporary, "cargo-calls")
  executable(File.join(fake_bin, "cargo"), <<~SH)
    test "${TEST_NATS_URL:-}" = 'tls://127.0.0.1:14222'
    test "${TEST_NATS_USERNAME:-}" = 'runtime-worker'
    test "${TEST_NATS_PASSWORD:-}" = 'worker-secret'
    test "${TEST_NATS_CA_CERT:-}" = '#{ca}'
    printf '%s\n' "$*" > '#{cargo_calls}'
  SH
  maven_calls = File.join(temporary, "maven-calls")
  executable(File.join(fake_bin, "mvn"), <<~SH)
    test "${TEST_NATS_URL:-}" = 'tls://127.0.0.1:14222'
    test "${TEST_NATS_USERNAME:-}" = 'control-plane'
    test "${TEST_NATS_PASSWORD:-}" = 'control-secret'
    test "${TEST_NATS_TRUSTSTORE:-}" = '#{truststore}'
    test "${TEST_NATS_TRUSTSTORE_PASSWORD:-}" = 'truststore-secret'
    printf '%s\n' "$*" > '#{maven_calls}'
  SH
  docker_marker = File.join(temporary, "docker-called")
  executable(File.join(fake_bin, "docker"), "printf called > '#{docker_marker}'\nexit 97\n")

  environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_DEVCTL" => File.join(fake_bin, "devctl"),
    "AGENT_RUNTIME_CARGO" => File.join(fake_bin, "cargo"),
    "AGENT_RUNTIME_MAVEN" => File.join(fake_bin, "mvn"),
    "PATH" => "#{fake_bin}:#{ENV.fetch('PATH')}"
  }
  output, error, status = Open3.capture3(environment, VERIFY, chdir: ROOT)

  raise "native NATS verification failed: #{output}#{error}" unless status.success?
  raise "native status was not checked" unless File.read(devctl_calls).include?("status")
  raise "Rust live TLS client was not exercised" unless File.file?(cargo_calls)
  raise "Java live TLS client was not exercised" unless File.file?(maven_calls)
  raise "Docker must never be invoked" if File.exist?(docker_marker)
end

puts "validated native-only NATS live verification contract"
