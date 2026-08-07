#!/usr/bin/env ruby
# frozen_string_literal: true

require "open3"

ROOT = File.expand_path("../..", __dir__)
targets = %w[
  test
  check
  check-native-dev
  dev
  dev-run
  dev-approve
  dev-native-bootstrap
  dev-native-start
  dev-status
  dev-down
  dev-clean
]
output, error, status = Open3.capture3("make", "--dry-run", *targets, chdir: ROOT)
raise "native make targets are invalid: #{output}#{error}" unless status.success?

forbidden = %w[docker compose kubectl minikube k3d kind]
forbidden.each do |command|
  raise "local command graph invokes #{command}" if output.match?(/(^|[\s\/])#{Regexp.escape(command)}([\s:]|$)/i)
end

docker_output, docker_error, docker_status = Open3.capture3(
  "make", "--dry-run", "build-images", chdir: ROOT
)
if docker_status.success?
  raise "Docker image build must not be exposed by the native development Makefile: #{docker_output}#{docker_error}"
end

unless output.include?("deploy/native/run-java-tests")
  raise "local Java tests must run through the native lifecycle supervisor"
end

%w[
  native_daemonize_service_test.rb
  native_clean_contract_test.rb
  native_approve_local_test.rb
  native_openai_tool_provider_test.rb
  openapi_approval_contract_test.rb
].each do |test_file|
  unless output.include?("ruby deploy/tests/#{test_file}")
    raise "native quality gate omits #{test_file}"
  end
end

approval_output, approval_error, approval_status = Open3.capture3(
  "make",
  "--dry-run",
  "dev-approve",
  "APPROVAL_ID=0198e899-e51e-7a0c-b3c3-07df25dfca45",
  chdir: ROOT
)
raise "native approval target is invalid: #{approval_output}#{approval_error}" unless approval_status.success?
approval_commands = approval_output.lines.map(&:strip).reject(&:empty?)
unless approval_commands == [
  "deploy/native/approve-local \"0198e899-e51e-7a0c-b3c3-07df25dfca45\" \"1\" \"allow_once\""
]
  raise "native approval target changed its secure command contract: #{approval_commands.inspect}"
end

run_output, run_error, run_status = Open3.capture3("make", "--dry-run", "dev-run", chdir: ROOT)
raise "native Run target is invalid: #{run_output}#{run_error}" unless run_status.success?
run_commands = run_output.lines.map(&:strip).reject(&:empty?)
unless run_commands == ["deploy/native/supervisor start", "deploy/native/run-local"]
  raise "one-command native Run must start the complete runtime before submitting a Run: #{run_commands.inspect}"
end

unless output.scan(/deploy\/native\/supervisor (?:start|status|stop|clean)/).uniq.sort == [
  "deploy/native/supervisor clean",
  "deploy/native/supervisor start",
  "deploy/native/supervisor status",
  "deploy/native/supervisor stop"
]
  raise "public local lifecycle targets must use the full native application supervisor"
end

raise "local Compose definition must not exist" if File.exist?(
  File.join(ROOT, "deploy", "local", "compose.yaml")
)

puts "validated zero-container local command graph"
