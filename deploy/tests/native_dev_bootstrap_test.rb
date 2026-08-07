#!/usr/bin/env ruby
# frozen_string_literal: true

require "digest"
require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
DEVCTL = File.join(ROOT, "deploy", "native", "devctl")

Dir.mktmpdir("agent-runtime-native-bootstrap-") do |temporary|
  fixture_root = File.join(temporary, "nats-server-v2.10.20-darwin-arm64")
  FileUtils.mkdir_p(fixture_root)
  fixture_binary = File.join(fixture_root, "nats-server")
  File.write(fixture_binary, "#!/bin/sh\nprintf 'nats-server: v2.10.20\\n'\n")
  FileUtils.chmod(0o755, fixture_binary)
  archive = File.join(temporary, "nats-server.tar.gz")
  raise "failed to build test archive" unless system(
    "tar", "-czf", archive, "-C", temporary, File.basename(fixture_root)
  )
  digest = Digest::SHA256.file(archive).hexdigest

  local_root = File.join(temporary, "local-success")
  environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_NATS_ARCHIVE_PATH" => archive,
    "AGENT_RUNTIME_NATS_ARCHIVE_SHA256" => digest
  }
  output, error, status = Open3.capture3(environment, DEVCTL, "bootstrap", chdir: ROOT)
  raise "bootstrap failed: #{output}#{error}" unless status.success?
  installed = File.join(local_root, "toolchain", "nats-server")
  raise "nats-server was not installed" unless File.executable?(installed)
  version, version_error, version_status = Open3.capture3(installed, "--version")
  raise "installed nats-server is invalid: #{version_error}" unless version_status.success?
  raise "unexpected nats-server version" unless version.include?("v2.10.20")
  raise "bootstrap archive leaked into local state" unless Dir.glob(File.join(local_root, "**", "*.tar.gz")).empty?

  failed_root = File.join(temporary, "local-failure")
  bad_environment = environment.merge(
    "AGENT_RUNTIME_LOCAL_ROOT" => failed_root,
    "AGENT_RUNTIME_NATS_ARCHIVE_SHA256" => "0" * 64
  )
  _bad_output, bad_error, bad_status = Open3.capture3(
    bad_environment, DEVCTL, "bootstrap", chdir: ROOT
  )
  raise "checksum mismatch must fail" if bad_status.success?
  raise "checksum failure must be explicit" unless bad_error.include?("checksum")
  raise "corrupt nats-server was installed" if File.exist?(
    File.join(failed_root, "toolchain", "nats-server")
  )

  go_bin = File.join(temporary, "go-bin")
  FileUtils.mkdir_p(go_bin)
  go_invocation = File.join(temporary, "go-invocation")
  fake_go = File.join(go_bin, "go")
  File.write(fake_go, <<~SH)
    #!/bin/sh
    set -eu
    printf 'GOBIN=%s\\nGOMODCACHE=%s\\nGOCACHE=%s\\nGOPROXY=%s\\nGOSUMDB=%s\\nHTTPS_PROXY=%s\\nARGS=%s\\n' \
      "$GOBIN" "$GOMODCACHE" "$GOCACHE" "$GOPROXY" "$GOSUMDB" "${HTTPS_PROXY:-}" "$*" > '#{go_invocation}'
    mkdir -p "$GOBIN"
    cp '#{fixture_binary}' "$GOBIN/nats-server"
    chmod 755 "$GOBIN/nats-server"
  SH
  FileUtils.chmod(0o755, fake_go)
  forbidden_curl = File.join(go_bin, "curl")
  File.write(forbidden_curl, "#!/bin/sh\nexit 99\n")
  FileUtils.chmod(0o755, forbidden_curl)
  go_root = File.join(temporary, "local-go")
  go_environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => go_root,
    "AGENT_RUNTIME_DOWNLOAD_PROXY" => "http://127.0.0.1:10808",
    # Explicit now that the archive is the default. This test is about the Go
    # path keeping its caches inside project state, not about which path `auto`
    # picks -- that is pinned separately below.
    "AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD" => "go",
    "PATH" => "#{go_bin}:#{ENV.fetch('PATH')}"
  }
  go_output, go_error, go_status = Open3.capture3(
    go_environment, DEVCTL, "bootstrap", chdir: ROOT
  )
  raise "Go bootstrap failed: #{go_output}#{go_error}" unless go_status.success?
  invocation = File.read(go_invocation)
  raise "Go binary escaped project state" unless invocation.include?("GOBIN=#{go_root}/toolchain")
  raise "Go module cache escaped project state" unless invocation.include?("GOMODCACHE=#{go_root}/cache/go/pkg/mod")
  raise "Go build cache escaped project state" unless invocation.include?("GOCACHE=#{go_root}/cache/go/build")
  raise "Go proxy must not inherit a machine-global mirror" unless invocation.include?(
    "GOPROXY=https://proxy.golang.org,direct"
  )
  raise "Go checksum database must remain enabled" unless invocation.include?("GOSUMDB=sum.golang.org")
  raise "project download proxy was not passed to Go" unless invocation.include?(
    "HTTPS_PROXY=http://127.0.0.1:10808"
  )
  raise "NATS version was not pinned" unless invocation.include?(
    "ARGS=install github.com/nats-io/nats-server/v2@v2.10.20"
  )

  fake_bin = File.join(temporary, "fake-bin")
  FileUtils.mkdir_p(fake_bin)
  curl_arguments = File.join(temporary, "curl-arguments")
  fake_curl = File.join(fake_bin, "curl")
  File.write(fake_curl, <<~SH)
    #!/bin/sh
    set -eu
    printf '%s\\n' "$@" > '#{curl_arguments}'
    output=''
    while [ "$#" -gt 0 ]; do
      [ "$1" = '-o' ] && output="$2" && break
      shift
    done
    cp '#{archive}' "$output"
  SH
  FileUtils.chmod(0o755, fake_curl)
  network_root = File.join(temporary, "local-network")
  network_environment = {
    "AGENT_RUNTIME_LOCAL_ROOT" => network_root,
    "AGENT_RUNTIME_NATS_ARCHIVE_SHA256" => digest,
    "AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD" => "archive",
    "PATH" => "#{fake_bin}:#{ENV.fetch('PATH')}"
  }
  network_output, network_error, network_status = Open3.capture3(
    network_environment, DEVCTL, "bootstrap", chdir: ROOT
  )
  raise "network bootstrap failed: #{network_output}#{network_error}" unless network_status.success?
  arguments = File.read(curl_arguments)
  %w[--connect-timeout --max-time --retry --retry-all-errors].each do |argument|
    raise "curl is missing bounded retry argument #{argument}" unless arguments.lines.map(&:chomp).include?(argument)
  end
  begin
    ordering_root = File.join(temporary, "local-ordering")
    ordering_bin = File.join(temporary, "ordering-bin")
    FileUtils.mkdir_p(ordering_bin)
    # A `go` that fails loudly if it is ever consulted.
    go_marker = File.join(temporary, "go-was-used")
    fake_go = File.join(ordering_bin, "go")
    File.write(fake_go, <<~SH)
      #!/bin/sh
      : > '#{go_marker}'
      exit 1
    SH
    FileUtils.chmod(0o755, fake_go)

    ordering_curl_arguments = File.join(temporary, "ordering-curl-arguments")
    fake_curl = File.join(ordering_bin, "curl")
    File.write(fake_curl, <<~SH)
      #!/bin/sh
      set -eu
      printf '%s\\n' "$@" > '#{ordering_curl_arguments}'
      output=''
      while [ "$#" -gt 0 ]; do
        [ "$1" = '-o' ] && output="$2" && break
        shift
      done
      cp '#{archive}' "$output"
    SH
    FileUtils.chmod(0o755, fake_curl)

    # No AGENT_RUNTIME_NATS_BOOTSTRAP_METHOD and no archive path: the default.
    ordering_output, ordering_error, ordering_status = Open3.capture3(
      {
        "AGENT_RUNTIME_LOCAL_ROOT" => ordering_root,
        "AGENT_RUNTIME_NATS_ARCHIVE_SHA256" => digest,
        "PATH" => "#{ordering_bin}:#{ENV.fetch('PATH')}"
      },
      DEVCTL, "bootstrap", chdir: ROOT
    )
    unless ordering_status.success?
      raise "default bootstrap failed: #{ordering_output}#{ordering_error}"
    end
    if File.exist?(go_marker)
      raise "the default bootstrap invoked go before trying the pinned archive"
    end
end

# `auto` must reach for the pinned archive before it reaches for a Go build.
#
# Both paths produce the same binary, but they do not fail the same way. The
# archive is one download with a SHA256 pinned in this repository. `go install`
# depends on proxy.golang.org and sum.golang.org being reachable and healthy at
# that moment, and on this network they were not: three attempts, three
# failures (connection reset, unexpected EOF, then a ten-minute hang), while the
# archive succeeded first try. `auto` preferring Go turned a working setup into
# a reported blocker.
#
# Pinned as an ordering, not as "Go is unavailable": the Go path stays, and
# stays selectable.
end

puts "validated native NATS bootstrap"
