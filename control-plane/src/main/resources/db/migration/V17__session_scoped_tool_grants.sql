alter table approvals
  add column policy_snapshot jsonb,
  add column policy_digest varchar(64),
  add column session_scope_digest varchar(64),
  add column session_grant_eligible boolean not null default false;

alter table approvals
  add constraint approvals_policy_snapshot_complete_check check (
    (policy_snapshot is null and policy_digest is null and session_scope_digest is null)
    or
    (policy_snapshot is not null and policy_digest is not null and session_scope_digest is not null)
  ),
  add constraint approvals_policy_snapshot_object_check
    check (policy_snapshot is null or jsonb_typeof(policy_snapshot) = 'object'),
  add constraint approvals_policy_digest_check
    check (policy_digest is null or policy_digest ~ '^[0-9a-f]{64}$'),
  add constraint approvals_session_scope_digest_check
    check (session_scope_digest is null or session_scope_digest ~ '^[0-9a-f]{64}$'),
  add constraint approvals_session_grant_eligible_check
    check (not session_grant_eligible or policy_snapshot is not null);

alter table runs
  add constraint runs_session_grant_source_unique
    unique (tenant_id,id,application_id,session_id,workspace_id,agent_version_id);

alter table approvals
  add constraint approvals_session_grant_source_unique
    unique (tenant_id,id,run_id);

create table session_tool_grants (
  tenant_id uuid not null,
  id uuid not null,
  source_run_id uuid not null,
  application_id uuid not null,
  session_id uuid not null,
  workspace_id uuid not null,
  agent_version_id uuid not null,
  scope_digest varchar(64) not null,
  policy_digest varchar(64) not null,
  policy_snapshot jsonb not null,
  tool_name varchar(256) not null,
  effect varchar(32) not null,
  sandbox varchar(32) not null,
  source_approval_id uuid not null,
  created_by varchar(255) not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint session_tool_grants_source_run_fk
    foreign key (
      tenant_id,source_run_id,application_id,session_id,workspace_id,agent_version_id
    ) references runs (
      tenant_id,id,application_id,session_id,workspace_id,agent_version_id
    ),
  constraint session_tool_grants_source_approval_fk
    foreign key (tenant_id, source_approval_id, source_run_id)
    references approvals (tenant_id, id, run_id),
  constraint session_tool_grants_scope_digest_check
    check (scope_digest ~ '^[0-9a-f]{64}$'),
  constraint session_tool_grants_policy_digest_check
    check (policy_digest ~ '^[0-9a-f]{64}$'),
  constraint session_tool_grants_policy_snapshot_check
    check (jsonb_typeof(policy_snapshot) = 'object'),
  constraint session_tool_grants_tool_name_check
    check (length(btrim(tool_name)) > 0 and length(tool_name) <= 256),
  constraint session_tool_grants_effect_check
    check (effect in ('pure', 'idempotent')),
  constraint session_tool_grants_unique
    unique (tenant_id, session_id, agent_version_id, scope_digest, policy_digest)
);

create index session_tool_grants_lookup_idx
  on session_tool_grants (
    tenant_id, application_id, session_id, workspace_id, agent_version_id,
    scope_digest, policy_digest
  );

alter table session_tool_grants enable row level security;
alter table session_tool_grants force row level security;
create policy tenant_isolation on session_tool_grants
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
