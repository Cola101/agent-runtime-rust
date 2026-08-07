#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
DEVCTL = File.join(ROOT, "deploy", "native", "devctl")

def executable(file_path, body = "exit 0\n")
  File.write(file_path, "#!/bin/sh\nset -eu\n#{body}")
  FileUtils.chmod(0o755, file_path)
end

Dir.mktmpdir("agent-runtime-native-clean-") do |temporary|
  project_root = File.join(temporary, "project")
  script_dir = File.join(project_root, "deploy", "native")
  FileUtils.mkdir_p(script_dir)
  copied_devctl = File.join(script_dir, "devctl")
  FileUtils.cp(DEVCTL, copied_devctl)
  FileUtils.chmod(0o755, copied_devctl)

  postgres_bin = File.join(temporary, "postgres-bin")
  FileUtils.mkdir_p(postgres_bin)
  %w[initdb pg_ctl psql createdb].each do |binary|
    executable(File.join(postgres_bin, binary))
  end

  local_root = File.join(project_root, ".local")
  readonly_module = File.join(local_root, "cache", "go", "pkg", "mod", "example@v1")
  FileUtils.mkdir_p(readonly_module)
  File.write(File.join(readonly_module, "module.go"), "package example\n")
  FileUtils.chmod(0o444, File.join(readonly_module, "module.go"))
  FileUtils.chmod(0o555, readonly_module)
  File.write(File.join(local_root, ".agent-runtime-local-root"), "")

  artifacts = %w[
    node_modules
    control-plane/target
    runtime/target
    console/node_modules
    console/dist
    console/coverage
    console/test-results
    console/playwright-report
    graphify-out
  ]
  artifacts.each do |relative|
    directory = File.join(project_root, relative)
    FileUtils.mkdir_p(directory)
    File.write(File.join(directory, "sentinel"), relative)
  end

  begin
    environment = { "AGENT_RUNTIME_POSTGRES_BIN_DIR" => postgres_bin }
    output, error, status = Open3.capture3(environment, copied_devctl, "clean", chdir: project_root)
    raise "clean failed: #{output}#{error}" unless status.success?
    raise "read-only project-local Go cache survived clean" if File.exist?(local_root)
    survivors = artifacts.select { |relative| File.exist?(File.join(project_root, relative)) }
    raise "clean left build or test outputs behind: #{survivors.join(', ')}" unless survivors.empty?

    protected_file = File.join(local_root, "cache", "protected")
    FileUtils.mkdir_p(File.dirname(protected_file))
    File.write(File.join(local_root, ".agent-runtime-local-root"), "")
    File.write(protected_file, "cannot remove yet\n")
    raise "failed to prepare immutable cleanup fixture" unless system("chflags", "uchg", protected_file)

    _failed_output, _failed_error, failed_status = Open3.capture3(
      environment, copied_devctl, "clean", chdir: project_root
    )
    raise "immutable cleanup fixture unexpectedly succeeded" if failed_status.success?
    unless File.file?(File.join(local_root, ".agent-runtime-local-root"))
      raise "failed clean removed the safety marker and made a retry impossible"
    end
    raise "failed to release immutable cleanup fixture" unless system(
      "chflags", "nouchg", protected_file
    )
  ensure
    system("chflags", "-R", "nouchg", local_root) if File.exist?(local_root)
    FileUtils.chmod_R(0o700, local_root) if File.exist?(local_root)
  end
end

puts "validated native build and test output cleanup"
