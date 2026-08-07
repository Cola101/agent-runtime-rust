alter table run_dispatches drop constraint run_dispatches_state_check;
alter table run_dispatches drop constraint run_dispatches_acceptance_check;

alter table run_dispatches
  add constraint run_dispatches_state_check check (
    state in ('requested', 'accepted', 'suspended', 'finished', 'lost')),
  add constraint run_dispatches_acceptance_check check (
    (state = 'requested' and accepted_at is null) or
    (state in ('accepted', 'suspended', 'finished') and accepted_at is not null) or
    state = 'lost');

create table subagent_calls (
  tenant_id uuid not null,
  parent_run_id uuid not null,
  parent_attempt_id uuid not null,
  tool_call_id varchar(256) not null,
  delegation_id uuid not null,
  role varchar(80) not null,
  input text not null,
  max_tokens bigint not null,
  max_cost_cents bigint not null,
  max_duration_seconds bigint not null,
  binding_digest varchar(64) not null,
  request_event_id uuid not null,
  request_sequence bigint not null,
  parent_checkpoint_id uuid,
  child_run_id uuid,
  state varchar(32) not null default 'awaiting_checkpoint',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, parent_run_id, tool_call_id),
  constraint subagent_calls_parent_dispatch_fk
    foreign key (tenant_id, parent_run_id, parent_attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  constraint subagent_calls_request_event_fk
    foreign key (tenant_id, request_event_id)
    references run_events (tenant_id, event_id),
  constraint subagent_calls_checkpoint_fk
    foreign key (tenant_id, parent_checkpoint_id)
    references run_checkpoints (tenant_id, checkpoint_id),
  constraint subagent_calls_child_run_fk
    foreign key (tenant_id, child_run_id)
    references runs (tenant_id, id),
  constraint subagent_calls_delegation_unique unique (tenant_id, delegation_id),
  constraint subagent_calls_request_sequence_unique
    unique (tenant_id, parent_run_id, request_sequence),
  constraint subagent_calls_state_check check (
    state in ('awaiting_checkpoint', 'child_queued', 'result_ready', 'delivered', 'cancelled')),
  constraint subagent_calls_budget_check check (
    max_tokens > 0 and max_cost_cents > 0 and
    max_duration_seconds between 1 and 86400),
  constraint subagent_calls_binding_digest_check check (
    binding_digest ~ '^[0-9a-f]{64}$'),
  constraint subagent_calls_identity_check check (
    role <> 'primary' and length(trim(input)) > 0 and request_sequence > 0),
  constraint subagent_calls_lifecycle_check check (
    (state = 'awaiting_checkpoint' and parent_checkpoint_id is null and child_run_id is null) or
    (state <> 'awaiting_checkpoint' and parent_checkpoint_id is not null and child_run_id is not null))
);

create index subagent_calls_child_state_idx
  on subagent_calls (tenant_id, child_run_id, state)
  where child_run_id is not null;

alter table subagent_calls enable row level security;
alter table subagent_calls force row level security;
create policy tenant_isolation on subagent_calls
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
