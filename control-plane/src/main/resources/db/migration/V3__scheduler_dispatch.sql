create table runtime_workers (
  id uuid primary key,
  placements varchar(16)[] not null,
  capacity integer not null,
  active_runs integer not null,
  runtime_version varchar(64) not null,
  last_heartbeat timestamptz not null,
  updated_at timestamptz not null default now(),
  constraint runtime_workers_placements_check check (
    cardinality(placements) > 0 and placements <@ array['cloud', 'edge']::varchar[]),
  constraint runtime_workers_capacity_check check (capacity > 0),
  constraint runtime_workers_active_runs_check check (
    active_runs >= 0 and active_runs <= capacity)
);

create table run_dispatches (
  tenant_id uuid not null,
  run_id uuid not null,
  attempt_id uuid not null,
  worker_id uuid not null,
  owner_epoch bigint not null,
  fencing_token uuid not null,
  lease_expires_at timestamptz not null,
  state varchar(24) not null default 'requested',
  requested_at timestamptz not null,
  accepted_at timestamptz,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, run_id),
  constraint run_dispatches_run_fk foreign key (tenant_id, run_id)
    references runs (tenant_id, id),
  constraint run_dispatches_worker_fk foreign key (worker_id)
    references runtime_workers (id),
  constraint run_dispatches_attempt_unique unique (attempt_id),
  constraint run_dispatches_fencing_unique unique (fencing_token),
  constraint run_dispatches_epoch_check check (owner_epoch > 0),
  constraint run_dispatches_state_check check (
    state in ('requested', 'accepted', 'finished', 'lost')),
  constraint run_dispatches_acceptance_check check (
    (state = 'requested' and accepted_at is null) or
    (state <> 'requested' and accepted_at is not null))
);

create index runtime_workers_schedulable_idx
  on runtime_workers (last_heartbeat desc)
  where active_runs < capacity;
create index run_dispatches_worker_state_idx
  on run_dispatches (worker_id, state, requested_at);

alter table run_dispatches enable row level security;
alter table run_dispatches force row level security;
create policy tenant_isolation on run_dispatches
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
