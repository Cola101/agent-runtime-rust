create table run_steering_commands (
  tenant_id uuid not null,
  application_id uuid not null,
  run_id uuid not null,
  steering_id uuid not null,
  idempotency_key varchar(128) not null,
  input text not null,
  input_digest varchar(64) not null,
  state varchar(24) not null default 'pending',
  attempt_id uuid not null,
  worker_id uuid not null,
  worker_incarnation_id uuid not null,
  requested_at timestamptz not null,
  issued_at timestamptz not null,
  expires_at timestamptz not null,
  applied_event_id uuid,
  created_at timestamptz not null default clock_timestamp(),
  updated_at timestamptz not null default clock_timestamp(),
  primary key (tenant_id, steering_id),
  constraint run_steering_commands_application_fk
    foreign key (tenant_id, application_id)
    references applications (tenant_id, id),
  constraint run_steering_commands_run_fk
    foreign key (tenant_id, run_id)
    references runs (tenant_id, id),
  constraint run_steering_commands_dispatch_fk
    foreign key (tenant_id, run_id, attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  constraint run_steering_commands_worker_incarnation_fk
    foreign key (worker_id, worker_incarnation_id)
    references runtime_worker_incarnations (worker_id, incarnation_id),
  constraint run_steering_commands_applied_event_fk
    foreign key (tenant_id, applied_event_id)
    references run_events (tenant_id, event_id),
  constraint run_steering_commands_idempotency_unique
    unique (tenant_id, application_id, run_id, idempotency_key),
  constraint run_steering_commands_input_check check (
    length(btrim(input)) > 0 and octet_length(input) <= 32768),
  constraint run_steering_commands_digest_check check (
    input_digest ~ '^[0-9a-f]{64}$'),
  constraint run_steering_commands_state_check check (
    state in ('pending', 'applied', 'rejected', 'cancelled')),
  constraint run_steering_commands_lifecycle_check check (
    (state = 'applied' and applied_event_id is not null)
    or (state <> 'applied' and applied_event_id is null)),
  constraint run_steering_commands_validity_check check (
    issued_at >= requested_at and expires_at > issued_at
    and expires_at <= issued_at + interval '5 minutes')
);

create unique index run_steering_commands_one_pending_run_idx
  on run_steering_commands (tenant_id, run_id)
  where state = 'pending';

create index run_steering_commands_pending_delivery_idx
  on run_steering_commands (tenant_id, expires_at, run_id)
  where state = 'pending';

alter table run_steering_commands enable row level security;
alter table run_steering_commands force row level security;
create policy tenant_isolation on run_steering_commands
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
