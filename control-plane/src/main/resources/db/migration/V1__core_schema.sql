create function current_tenant_id() returns uuid
language sql stable
return nullif(current_setting('app.tenant_id', true), '')::uuid;

create table tenants (
  tenant_id uuid not null,
  id uuid not null,
  slug varchar(63) not null,
  display_name varchar(200) not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint tenants_id_matches_scope check (tenant_id = id),
  constraint tenants_slug_unique unique (slug)
);

create table applications (
  tenant_id uuid not null,
  id uuid not null,
  name varchar(200) not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint applications_tenant_fk foreign key (tenant_id, tenant_id)
    references tenants (tenant_id, id)
);

create table projects (
  tenant_id uuid not null,
  id uuid not null,
  application_id uuid not null,
  name varchar(200) not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint projects_tenant_application_fk foreign key (tenant_id, application_id)
    references applications (tenant_id, id)
);

create table workspaces (
  tenant_id uuid not null,
  id uuid not null,
  project_id uuid not null,
  name varchar(200) not null,
  state varchar(32) not null default 'ready',
  baseline_digest varchar(128),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint workspaces_project_fk foreign key (tenant_id, project_id)
    references projects (tenant_id, id),
  constraint workspaces_state_check check (state in ('ready', 'leased', 'conflicted', 'deleted'))
);

create table agents (
  tenant_id uuid not null,
  id uuid not null,
  workspace_id uuid not null,
  name varchar(200) not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint agents_workspace_fk foreign key (tenant_id, workspace_id)
    references workspaces (tenant_id, id)
);

create table agent_versions (
  tenant_id uuid not null,
  id uuid not null,
  agent_id uuid not null,
  version integer not null,
  spec jsonb not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint agent_versions_agent_fk foreign key (tenant_id, agent_id)
    references agents (tenant_id, id),
  constraint agent_versions_number_check check (version > 0),
  constraint agent_versions_unique unique (tenant_id, agent_id, version)
);

create table sessions (
  tenant_id uuid not null,
  id uuid not null,
  workspace_id uuid not null,
  state varchar(32) not null default 'active',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint sessions_workspace_fk foreign key (tenant_id, workspace_id)
    references workspaces (tenant_id, id),
  constraint sessions_state_check check (state in ('active', 'suspended', 'closed'))
);

create table runs (
  tenant_id uuid not null,
  id uuid not null,
  session_id uuid not null,
  workspace_id uuid not null,
  agent_version_id uuid not null,
  idempotency_key varchar(128) not null,
  input text not null,
  status varchar(32) not null,
  priority smallint not null default 0,
  placement varchar(32) not null default 'cloud',
  max_tokens bigint not null,
  max_cost_cents bigint not null,
  max_duration_seconds bigint not null,
  current_attempt_id uuid,
  last_sequence bigint not null default 0,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  finished_at timestamptz,
  primary key (tenant_id, id),
  constraint runs_session_fk foreign key (tenant_id, session_id)
    references sessions (tenant_id, id),
  constraint runs_workspace_fk foreign key (tenant_id, workspace_id)
    references workspaces (tenant_id, id),
  constraint runs_agent_version_fk foreign key (tenant_id, agent_version_id)
    references agent_versions (tenant_id, id),
  constraint runs_idempotency_unique unique (tenant_id, idempotency_key),
  constraint runs_status_check check (status in (
    'queued', 'running', 'waiting_approval', 'suspended', 'succeeded',
    'failed', 'cancelled', 'timed_out', 'indeterminate')),
  constraint runs_placement_check check (placement in ('cloud', 'edge', 'any')),
  constraint runs_budget_check check (
    max_tokens > 0 and max_cost_cents > 0 and
    max_duration_seconds > 0 and max_duration_seconds <= 86400),
  constraint runs_sequence_check check (last_sequence >= 0)
);

create table run_events (
  tenant_id uuid not null,
  event_id uuid not null,
  run_id uuid not null,
  session_id uuid not null,
  sequence bigint not null,
  schema_version integer not null,
  attempt_id uuid not null,
  occurred_at timestamptz not null,
  trace_id varchar(64) not null,
  type varchar(100) not null,
  payload jsonb,
  payload_ref text,
  digest varchar(128) not null,
  committed_at timestamptz not null default now(),
  primary key (tenant_id, event_id),
  constraint run_events_run_fk foreign key (tenant_id, run_id)
    references runs (tenant_id, id),
  constraint run_events_session_fk foreign key (tenant_id, session_id)
    references sessions (tenant_id, id),
  constraint run_events_sequence_unique unique (tenant_id, run_id, sequence),
  constraint run_events_sequence_check check (sequence > 0),
  constraint run_events_schema_check check (schema_version > 0),
  constraint run_events_payload_check check (
    (payload is not null and payload_ref is null) or
    (payload is null and payload_ref is not null))
);

create table approvals (
  tenant_id uuid not null,
  id uuid not null,
  run_id uuid not null,
  version integer not null default 1,
  status varchar(20) not null default 'pending',
  request jsonb not null,
  decision jsonb,
  decided_by varchar(255),
  created_at timestamptz not null default now(),
  decided_at timestamptz,
  primary key (tenant_id, id),
  constraint approvals_run_fk foreign key (tenant_id, run_id)
    references runs (tenant_id, id),
  constraint approvals_status_check check (status in ('pending', 'approved', 'denied', 'expired')),
  constraint approvals_version_check check (version > 0)
);

create table workspace_leases (
  tenant_id uuid not null,
  workspace_id uuid not null,
  owner_id uuid not null,
  owner_epoch bigint not null,
  fencing_token uuid not null,
  expires_at timestamptz not null,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, workspace_id),
  constraint workspace_leases_workspace_fk foreign key (tenant_id, workspace_id)
    references workspaces (tenant_id, id),
  constraint workspace_leases_epoch_check check (owner_epoch > 0),
  constraint workspace_leases_token_unique unique (fencing_token)
);

create table outbox_events (
  tenant_id uuid not null,
  id uuid not null,
  aggregate_type varchar(64) not null,
  aggregate_id uuid not null,
  event_type varchar(100) not null,
  payload jsonb not null,
  created_at timestamptz not null default now(),
  published_at timestamptz,
  publish_attempts integer not null default 0,
  primary key (tenant_id, id),
  constraint outbox_attempts_check check (publish_attempts >= 0)
);

create index projects_application_idx on projects (tenant_id, application_id);
create index workspaces_project_idx on workspaces (tenant_id, project_id);
create index agents_workspace_idx on agents (tenant_id, workspace_id);
create index sessions_workspace_idx on sessions (tenant_id, workspace_id);
create index runs_session_created_idx on runs (tenant_id, session_id, created_at desc);
create index runs_status_created_idx on runs (tenant_id, status, created_at);
create index run_events_resume_idx on run_events (tenant_id, run_id, sequence);
create index approvals_pending_idx on approvals (tenant_id, status, created_at)
  where status = 'pending';
create index outbox_unpublished_idx on outbox_events (created_at)
  where published_at is null;

do $rls$
declare
  table_name text;
begin
  foreach table_name in array array[
    'tenants', 'applications', 'projects', 'workspaces', 'agents', 'agent_versions',
    'sessions', 'runs', 'run_events', 'approvals', 'workspace_leases', 'outbox_events'
  ]
  loop
    execute format('alter table %I enable row level security', table_name);
    execute format('alter table %I force row level security', table_name);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = current_tenant_id()) with check (tenant_id = current_tenant_id())',
      table_name);
  end loop;
end
$rls$;
