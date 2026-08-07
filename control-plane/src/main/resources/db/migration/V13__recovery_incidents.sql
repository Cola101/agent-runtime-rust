alter table runtime_worker_incarnations
  add column last_heartbeat_received_at timestamptz not null default clock_timestamp();

drop index runtime_worker_incarnations_schedulable_idx;
create index runtime_worker_incarnations_schedulable_idx
  on runtime_worker_incarnations (last_heartbeat_received_at desc)
  where accepting_work and active_runs < capacity;

create table recovery_incidents (
  tenant_id uuid not null,
  incident_id uuid not null,
  run_id uuid not null,
  failed_attempt_id uuid not null,
  failed_worker_id uuid not null,
  failed_worker_incarnation_id uuid not null,
  recovery_attempt_id uuid,
  last_confirmed_healthy_at timestamptz not null,
  detected_at timestamptz not null default clock_timestamp(),
  state varchar(32) not null,
  resolved_at timestamptz,
  updated_at timestamptz not null default clock_timestamp(),
  primary key (tenant_id, incident_id),
  constraint recovery_incidents_run_fk foreign key (tenant_id, run_id)
    references runs (tenant_id, id),
  constraint recovery_incidents_failed_dispatch_fk
    foreign key (tenant_id, run_id, failed_attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  constraint recovery_incidents_recovery_dispatch_fk
    foreign key (tenant_id, run_id, recovery_attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  constraint recovery_incidents_failed_worker_incarnation_fk
    foreign key (failed_worker_id, failed_worker_incarnation_id)
    references runtime_worker_incarnations (worker_id, incarnation_id),
  constraint recovery_incidents_state_check check (
    state in (
      'waiting_capacity', 'recovery_requested', 'recovered', 'terminated', 'indeterminate')),
  constraint recovery_incidents_resolution_check check (
    (state in ('waiting_capacity', 'recovery_requested') and resolved_at is null)
    or
    (state in ('recovered', 'terminated', 'indeterminate') and resolved_at is not null)),
  constraint recovery_incidents_recovery_attempt_check check (
    state not in ('recovery_requested', 'recovered') or recovery_attempt_id is not null),
  constraint recovery_incidents_time_check check (
    detected_at >= last_confirmed_healthy_at
    and (resolved_at is null or resolved_at >= detected_at))
);

create unique index recovery_incidents_one_open_run_idx
  on recovery_incidents (tenant_id, run_id)
  where resolved_at is null;

create index recovery_incidents_open_slo_idx
  on recovery_incidents (tenant_id, last_confirmed_healthy_at)
  where resolved_at is null;

alter table recovery_incidents enable row level security;
alter table recovery_incidents force row level security;
create policy tenant_isolation on recovery_incidents
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
