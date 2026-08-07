#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "open3"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
SEEDER = File.join(ROOT, "deploy", "native", "seed-local-development")

Dir.mktmpdir("agent-runtime-seed-test-") do |temporary|
  local_root = File.join(temporary, ".local")
  fake_bin = File.join(temporary, "bin")
  captured_arguments = File.join(temporary, "psql-arguments")
  captured_sql = File.join(temporary, "seed.sql")
  FileUtils.mkdir_p([File.join(local_root, "env"), fake_bin])
  identity_root = File.join(local_root, "secrets", "identity")
  FileUtils.mkdir_p(identity_root)
  system("openssl", "genpkey", "-algorithm", "ED25519", "-out",
         File.join(identity_root, "skill-private.pem"), out: File::NULL, err: File::NULL) ||
    raise("could not create test Skill key")
  File.write(File.join(local_root, ".agent-runtime-local-root"), "")
  File.write(File.join(local_root, "env", "native.env"), <<~ENVIRONMENT)
    export SPRING_DATASOURCE_URL='jdbc:postgresql://127.0.0.1:54329/agent_runtime'
    export SPRING_DATASOURCE_USERNAME='agent_runtime_owner'
    export SPRING_DATASOURCE_PASSWORD='postgres-secret'
  ENVIRONMENT
  psql = File.join(fake_bin, "psql")
  File.write(psql, <<~'SH')
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" > "$CAPTURED_ARGUMENTS"
    [ "$PGPASSWORD" = 'postgres-secret' ] || exit 91
    while [ "$#" -gt 0 ]; do
      if [ "$1" = '-f' ]; then cp "$2" "$CAPTURED_SQL"; exit 0; fi
      shift
    done
    exit 92
  SH
  FileUtils.chmod(0o755, psql)
  configured_provider = File.join(temporary, "configured-provider")
  provider_configurer = File.join(fake_bin, "configure-local-model-provider")
  File.write(provider_configurer, <<~'SH')
    #!/bin/sh
    set -eu
    [ "${SPRING_DATASOURCE_PASSWORD:-}" = 'postgres-secret' ] || exit 93
    : > "$CONFIGURED_PROVIDER"
  SH
  FileUtils.chmod(0o755, provider_configurer)

  output, error, status = Open3.capture3({
    "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
    "AGENT_RUNTIME_PSQL_BIN" => psql,
    "CAPTURED_ARGUMENTS" => captured_arguments,
    "CAPTURED_SQL" => captured_sql,
    "CONFIGURED_PROVIDER" => configured_provider,
    "AGENT_RUNTIME_MODEL_PROVIDER_CONFIGURER" => provider_configurer,
  }, SEEDER, chdir: ROOT)
  raise "native seed failed: #{output}#{error}" unless status.success?
  raise "database password leaked into argv" if File.read(captured_arguments).include?('postgres-secret')
  raise "native model Provider was not configured after base resources" unless File.file?(configured_provider)

  sql = File.read(captured_sql)
  {
    tenants: '11111111-1111-4111-8111-111111111111',
    applications: '22222222-2222-4222-8222-222222222222',
    projects: '33333333-3333-4333-8333-333333333333',
    workspaces: '44444444-4444-4444-8444-444444444444',
    agents: '55555555-5555-4555-8555-555555555555',
    agent_versions: '66666666-6666-4666-8666-666666666666',
    skills: '99999999-9999-4999-8999-999999999999',
    skill_versions: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
    agent_version_skills: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
    sessions: '77777777-7777-4777-8777-777777777777',
    model_policies: '88888888-8888-4888-8888-888888888888',
  }.each do |table, identifier|
    raise "missing idempotent #{table} seed" unless sql.include?("insert into #{table}") && sql.include?(identifier)
  end
  raise "seed must set the RLS tenant context" unless sql.include?("set_config('app.tenant_id'")
  raise "seed must be transactional" unless sql.include?("begin;") && sql.include?("commit;")
  raise "seed must be repeatable" unless sql.scan(/on conflict/).length >= 8
  raise "native Agent must delegate only the trusted workspace read scope" unless sql.include?(
    '"delegated_scopes":["tool:workspace.read"]'
  )
  raise "native AgentVersion must bind the signed SkillVersion" unless sql.include?(
    "insert into agent_version_skills"
  ) && File.read(captured_arguments).include?("skill_artifact_digest=") &&
      File.read(captured_arguments).include?("skill_signature=")
end

puts "validated repeatable native multi-tenant development seed"
