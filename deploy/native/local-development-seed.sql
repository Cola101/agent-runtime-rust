begin;

select set_config('app.tenant_id', '11111111-1111-4111-8111-111111111111', true);

insert into tenants (tenant_id, id, slug, display_name)
values (
  '11111111-1111-4111-8111-111111111111',
  '11111111-1111-4111-8111-111111111111',
  'local-development',
  'Local Development Tenant')
on conflict (tenant_id, id) do update
  set display_name = excluded.display_name,
      updated_at = now();

insert into applications (tenant_id, id, name)
values (
  '11111111-1111-4111-8111-111111111111',
  '22222222-2222-4222-8222-222222222222',
  'Local Agent Runtime')
on conflict (tenant_id, id) do update
  set name = excluded.name,
      updated_at = now();

insert into projects (tenant_id, id, application_id, name)
values (
  '11111111-1111-4111-8111-111111111111',
  '33333333-3333-4333-8333-333333333333',
  '22222222-2222-4222-8222-222222222222',
  'Native Development')
on conflict (tenant_id, id) do update
  set application_id = excluded.application_id,
      name = excluded.name,
      updated_at = now();

insert into workspaces (tenant_id, id, project_id, name)
values (
  '11111111-1111-4111-8111-111111111111',
  '44444444-4444-4444-8444-444444444444',
  '33333333-3333-4333-8333-333333333333',
  'Local Workspace')
on conflict (tenant_id, id) do update
  set project_id = excluded.project_id,
      name = excluded.name,
      updated_at = now();

insert into agents (tenant_id, id, workspace_id, name)
values (
  '11111111-1111-4111-8111-111111111111',
  '55555555-5555-4555-8555-555555555555',
  '44444444-4444-4444-8444-444444444444',
  'Local Runtime Agent')
on conflict (tenant_id, id) do update
  set workspace_id = excluded.workspace_id,
      name = excluded.name,
      updated_at = now();

insert into agent_versions (tenant_id, id, application_id, agent_id, version, spec)
values (
  '11111111-1111-4111-8111-111111111111',
  '66666666-6666-4666-8666-666666666666',
  '22222222-2222-4222-8222-222222222222',
  '55555555-5555-4555-8555-555555555555',
  1,
  '{"instructions":"Help the user from the local native runtime.","delegated_scopes":["tool:workspace.read"]}'::jsonb)
on conflict (tenant_id, id) do update
  set agent_id = excluded.agent_id,
      application_id = excluded.application_id,
      version = excluded.version,
      spec = excluded.spec;

insert into skills (tenant_id, id, application_id, name)
values (
  '11111111-1111-4111-8111-111111111111',
  '99999999-9999-4999-8999-999999999999',
  '22222222-2222-4222-8222-222222222222',
  'workspace-review')
on conflict (tenant_id, id) do update
  set name = excluded.name,
      updated_at = now();

insert into skill_versions (
  tenant_id, id, application_id, skill_id, semantic_version, artifact,
  artifact_digest, signing_key_id, signature)
values (
  '11111111-1111-4111-8111-111111111111',
  'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
  '22222222-2222-4222-8222-222222222222',
  '99999999-9999-4999-8999-999999999999',
  '1.0.0',
  jsonb_build_object(
    'schema_version', 1,
    'tenant_id', '11111111-1111-4111-8111-111111111111',
    'application_id', '22222222-2222-4222-8222-222222222222',
    'skill_version_id', 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
    'name', 'workspace-review',
    'semantic_version', '1.0.0',
    'description', 'Review bounded workspace evidence.',
    'instructions', 'Read workspace evidence before answering.',
    'tool_names', jsonb_build_array('workspace.read_text'),
    'supported_platforms', jsonb_build_array('darwin-arm64','linux-arm64','linux-x86_64'),
    'min_runtime_version', '0.1.0'),
  :'skill_artifact_digest',
  'local-skill-key-v1',
  :'skill_signature')
on conflict (tenant_id, id) do nothing;

insert into agent_version_skills (
  tenant_id, application_id, agent_version_id, ordinal, skill_version_id, artifact_digest)
values (
  '11111111-1111-4111-8111-111111111111',
  '22222222-2222-4222-8222-222222222222',
  '66666666-6666-4666-8666-666666666666',
  0,
  'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
  :'skill_artifact_digest')
on conflict (tenant_id, agent_version_id, ordinal) do nothing;

insert into sessions (tenant_id, id, workspace_id)
values (
  '11111111-1111-4111-8111-111111111111',
  '77777777-7777-4777-8777-777777777777',
  '44444444-4444-4444-8444-444444444444')
on conflict (tenant_id, id) do update
  set workspace_id = excluded.workspace_id,
      state = 'active',
      updated_at = now();

insert into model_policies (tenant_id, id, workspace_id, name, policy, application_id)
values (
  '11111111-1111-4111-8111-111111111111',
  '88888888-8888-4888-8888-888888888888',
  '44444444-4444-4444-8444-444444444444',
  'Native Model Gateway',
  '{"routing":"single_provider"}'::jsonb,
  '22222222-2222-4222-8222-222222222222')
on conflict (tenant_id, id) do update
  set workspace_id = excluded.workspace_id,
      name = excluded.name,
      policy = excluded.policy,
      application_id = excluded.application_id,
      updated_at = now();

commit;
