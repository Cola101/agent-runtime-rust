create table tool_executions (
  tenant_id uuid not null,
  run_id uuid not null,
  attempt_id uuid not null,
  tool_call_id varchar(256) not null,
  binding_digest varchar(64) not null,
  effect varchar(24) not null,
  sandbox varchar(32) not null,
  state varchar(24) not null default 'planned',
  request jsonb not null,
  requested_event_id uuid not null,
  started_event_id uuid,
  result_event_id uuid,
  requested_at timestamptz not null,
  started_at timestamptz,
  completed_at timestamptz,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, run_id, attempt_id, tool_call_id),
  constraint tool_executions_dispatch_fk
    foreign key (tenant_id, run_id, attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  constraint tool_executions_requested_event_fk
    foreign key (tenant_id, requested_event_id)
    references run_events (tenant_id, event_id),
  constraint tool_executions_started_event_fk
    foreign key (tenant_id, started_event_id)
    references run_events (tenant_id, event_id),
  constraint tool_executions_result_event_fk
    foreign key (tenant_id, result_event_id)
    references run_events (tenant_id, event_id),
  constraint tool_executions_call_check
    check (length(btrim(tool_call_id)) > 0 and length(tool_call_id) <= 256),
  constraint tool_executions_binding_check
    check (binding_digest ~ '^[0-9a-f]{64}$'),
  constraint tool_executions_effect_check
    check (effect in ('pure', 'idempotent', 'non_idempotent', 'unknown')),
  constraint tool_executions_sandbox_check
    check (sandbox in ('restricted_container', 'kata')),
  constraint tool_executions_state_check
    check (state in ('planned', 'started', 'completed')),
  constraint tool_executions_lifecycle_check check (
    (state = 'planned' and started_event_id is null and result_event_id is null
      and started_at is null and completed_at is null) or
    (state = 'started' and started_event_id is not null and result_event_id is null
      and started_at is not null and completed_at is null) or
    (state = 'completed' and result_event_id is not null and completed_at is not null)
  )
);

create unique index tool_executions_requested_event_unique
  on tool_executions (tenant_id, requested_event_id);
create unique index tool_executions_started_event_unique
  on tool_executions (tenant_id, started_event_id)
  where started_event_id is not null;
create unique index tool_executions_result_event_unique
  on tool_executions (tenant_id, result_event_id)
  where result_event_id is not null;
create index tool_executions_open_idx
  on tool_executions (tenant_id, run_id, attempt_id, state)
  where state <> 'completed';

alter table tool_executions enable row level security;
alter table tool_executions force row level security;
create policy tenant_isolation on tool_executions
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());

