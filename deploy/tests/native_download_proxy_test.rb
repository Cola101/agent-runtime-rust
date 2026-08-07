#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
WRAPPER = File.join(ROOT, "deploy", "native", "with-download-proxy")

def executable(path, body)
  File.write(path, "#!/bin/sh\nset -eu\n#{body}")
  FileUtils.chmod(0o755, path)
end

Dir.mktmpdir("agent-runtime-proxy-test-") do |temporary|
  fake_bin = File.join(temporary, "bin")
  FileUtils.mkdir_p(fake_bin)
  executable(File.join(fake_bin, "scutil"), <<~'SH')
    cat <<'PROXY'
    <dictionary> {
      HTTPEnable : 1
      HTTPPort : 10808
      HTTPProxy : 127.0.0.1
    }
    PROXY
  SH
  captured = File.join(temporary, "captured")
  executable(File.join(fake_bin, "capture-environment"), <<~SH)
    printf '%s\n' \
      "HTTP_PROXY=${HTTP_PROXY:-}" \
      "HTTPS_PROXY=${HTTPS_PROXY:-}" \
      "ALL_PROXY=${ALL_PROXY:-}" \
      "http_proxy=${http_proxy:-}" \
      "https_proxy=${https_proxy:-}" \
      "all_proxy=${all_proxy:-}" \
      "NO_PROXY=${NO_PROXY:-}" \
      "no_proxy=${no_proxy:-}" \
      "MAVEN_OPTS=${MAVEN_OPTS:-}" \
      "ARGS=$*" > '#{captured}'
  SH

  environment = {
    "PATH" => "#{fake_bin}:#{ENV.fetch('PATH')}",
    "NO_PROXY" => "internal.example",
    "MAVEN_OPTS" => "-Xmx768m"
  }
  output, error, status = Open3.capture3(
    environment, WRAPPER, "capture-environment", "dependency", "check", chdir: ROOT
  )
  raise "proxy wrapper failed: #{output}#{error}" unless status.success?

  values = File.readlines(captured, chomp: true).to_h { |line| line.split("=", 2) }
  %w[HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy].each do |name|
    raise "#{name} did not use the macOS proxy" unless values[name] == "http://127.0.0.1:10808"
  end
  %w[localhost 127.0.0.1 ::1 .local internal.example].each do |host|
    raise "NO_PROXY omitted #{host}" unless values.fetch("NO_PROXY").split(",").include?(host)
    raise "no_proxy omitted #{host}" unless values.fetch("no_proxy").split(",").include?(host)
  end
  maven_options = values.fetch("MAVEN_OPTS")
  raise "existing MAVEN_OPTS were discarded" unless maven_options.include?("-Xmx768m")
  raise "Maven HTTP proxy host missing" unless maven_options.include?("-Dhttp.proxyHost=127.0.0.1")
  raise "Maven HTTPS proxy port missing" unless maven_options.include?("-Dhttps.proxyPort=10808")
  raise "Maven loopback bypass missing" unless maven_options.include?(
    "-Dhttp.nonProxyHosts=localhost|127.*|[::1]|*.local"
  )
  raise "wrapped command arguments changed" unless values["ARGS"] == "dependency check"
end

# A machine with no macOS proxy configured must still build. The wrapper is the
# single entry point for every dependency download, so failing closed here takes
# out `make dev` entirely on any host that does not run a local proxy.
def run_without_proxy(scutil_body, environment_overrides, description)
  Dir.mktmpdir("agent-runtime-proxy-direct-test-") do |temporary|
    fake_bin = File.join(temporary, "bin")
    FileUtils.mkdir_p(fake_bin)
    executable(File.join(fake_bin, "scutil"), scutil_body)
    captured = File.join(temporary, "captured")
    executable(File.join(fake_bin, "capture-environment"), <<~SH)
      printf '%s\n' \
        "HTTP_PROXY=${HTTP_PROXY:-}" \
        "HTTPS_PROXY=${HTTPS_PROXY:-}" \
        "NO_PROXY=${NO_PROXY:-}" \
        "MAVEN_OPTS=${MAVEN_OPTS:-}" \
        "ARGS=$*" > '#{captured}'
    SH

    environment = { "PATH" => "#{fake_bin}:#{ENV.fetch('PATH')}" }.merge(environment_overrides)
    output, error, status = Open3.capture3(
      environment, WRAPPER, "capture-environment", "dependency", "check", chdir: ROOT
    )
    unless status.success?
      raise "#{description}: wrapper refused to run the command " \
            "(exit #{status.exitstatus}) #{output}#{error}"
    end
    raise "#{description}: wrapped command never ran" unless File.file?(captured)

    values = File.readlines(captured, chomp: true).to_h { |line| line.split("=", 2) }
    %w[HTTP_PROXY HTTPS_PROXY].each do |name|
      raise "#{description}: #{name} must stay unset" unless values[name].to_s.empty?
    end
    raise "#{description}: Maven proxy options leaked" unless values["MAVEN_OPTS"].to_s.empty?
    %w[localhost 127.0.0.1 ::1 .local].each do |host|
      raise "#{description}: NO_PROXY omitted #{host}" \
        unless values.fetch("NO_PROXY").split(",").include?(host)
    end
    raise "#{description}: wrapped command arguments changed" \
      unless values["ARGS"] == "dependency check"
  end
end

run_without_proxy(<<~'SH', {}, "macOS proxy disabled")
  cat <<'PROXY'
  <dictionary> {
    HTTPEnable : 0
    HTTPSEnable : 0
    SOCKSEnable : 0
  }
  PROXY
SH

run_without_proxy(<<~'SH', { "AGENT_RUNTIME_DOWNLOAD_PROXY" => "direct" }, "explicit direct opt-out")
  cat <<'PROXY'
  <dictionary> {
    HTTPEnable : 1
    HTTPPort : 10808
    HTTPProxy : 127.0.0.1
  }
  PROXY
SH

puts "validated native download proxy propagation, loopback bypass, and direct fallback"
