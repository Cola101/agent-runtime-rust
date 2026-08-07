#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
DAEMONIZER = File.join(ROOT, "deploy", "native", "daemonize-service")

def process_alive?(pid)
  Process.kill(0, pid)
  true
rescue Errno::ESRCH
  false
end

Dir.mktmpdir("agent-runtime-daemonizer-test-") do |temporary|
  local_root = File.join(temporary, ".local")
  FileUtils.mkdir_p([File.join(local_root, "run"), File.join(local_root, "logs")])
  File.write(File.join(local_root, ".agent-runtime-local-root"), "managed\n")
  executable = File.join(temporary, "service")
  File.write(executable, <<~'SH')
    #!/bin/sh
    set -eu
    printf 'ready:%s:%s\n' "$SERVICE_PROBE" "$1"
    trap 'exit 0' TERM INT
    while :; do sleep 1; done
  SH
  FileUtils.chmod(0o755, executable)

  output, error, status = Open3.capture3(
    { "SERVICE_PROBE" => "environment-preserved" },
    "/usr/bin/ruby", DAEMONIZER, "probe", local_root, executable, "argument-preserved"
  )
  raise "daemonizer failed: #{output}#{error}" unless status.success?

  pid_file = File.join(local_root, "run", "probe.pid")
  pgid_file = File.join(local_root, "run", "probe.pgid")
  stdout_file = File.join(local_root, "logs", "probe.stdout.log")
  attempts = 0
  until File.size?(pid_file) && File.size?(pgid_file) && File.size?(stdout_file)
    sleep 0.05
    attempts += 1
    raise "daemonized service did not become observable" if attempts >= 40
  end

  pid = Integer(File.read(pid_file).strip, 10)
  pgid = Integer(File.read(pgid_file).strip, 10)
  raise "daemon did not survive its launcher" unless process_alive?(pid)
  daemon_ppid = Integer(`ps -o ppid= -p #{pid}`.strip, 10)
  raise "daemon was not reparented" unless daemon_ppid == 1
  raise "daemon reused the test process group" if pgid == Process.getpgrp
  unless File.read(stdout_file).include?("ready:environment-preserved:argument-preserved")
    raise "daemon lost its environment, arguments, or log redirection"
  end
  raise "daemon PID permissions are too broad" unless File.stat(pid_file).mode & 0o077 == 0
  raise "daemon PGID permissions are too broad" unless File.stat(pgid_file).mode & 0o077 == 0

  Process.kill("TERM", -pgid)
  attempts = 0
  while process_alive?(pid) && attempts < 40
    sleep 0.05
    attempts += 1
  end
  raise "daemon process group did not stop" if process_alive?(pid)
end

puts "validated detached native process group lifecycle"
