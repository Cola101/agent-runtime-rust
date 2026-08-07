create table runtime_worker_incarnations (
  worker_id uuid not null,
  incarnation_id uuid not null,
  placements varchar(16)[] not null,
  capacity integer not null,
  active_runs integer not null,
  runtime_version varchar(64) not null,
  last_heartbeat timestamptz not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (worker_id, incarnation_id),
  constraint runtime_worker_incarnations_worker_fk foreign key (worker_id)
    references runtime_workers (id),
  constraint runtime_worker_incarnations_placements_check check (
    cardinality(placements) > 0 and placements <@ array['cloud', 'edge']::varchar[]),
  constraint runtime_worker_incarnations_capacity_check check (capacity > 0),
  constraint runtime_worker_incarnations_active_runs_check check (
    active_runs >= 0 and active_runs <= capacity)
);

insert into runtime_worker_incarnations (
  worker_id, incarnation_id, placements, capacity, active_runs, runtime_version, last_heartbeat)
select id, id, placements, capacity, active_runs, runtime_version, last_heartbeat
  from runtime_workers;

alter table runtime_workers add column current_incarnation_id uuid;
update runtime_workers set current_incarnation_id = id;
alter table runtime_workers alter column current_incarnation_id set not null;
alter table runtime_workers
  add constraint runtime_workers_current_incarnation_fk
    foreign key (id, current_incarnation_id)
    references runtime_worker_incarnations (worker_id, incarnation_id)
    deferrable initially deferred;

alter table run_dispatches add column worker_incarnation_id uuid;
update run_dispatches set worker_incarnation_id = worker_id;
alter table run_dispatches alter column worker_incarnation_id set not null;
alter table run_dispatches
  add constraint run_dispatches_worker_incarnation_fk
    foreign key (worker_id, worker_incarnation_id)
    references runtime_worker_incarnations (worker_id, incarnation_id);

alter table approvals add column worker_incarnation_id uuid;
update approvals a
   set worker_incarnation_id = d.worker_incarnation_id
  from run_dispatches d
 where a.tenant_id = d.tenant_id and a.run_id = d.run_id
   and a.attempt_id = d.attempt_id and a.worker_id = d.worker_id;
alter table approvals
  add constraint approvals_worker_incarnation_fk
    foreign key (worker_id, worker_incarnation_id)
    references runtime_worker_incarnations (worker_id, incarnation_id);

create index runtime_worker_incarnations_schedulable_idx
  on runtime_worker_incarnations (last_heartbeat desc)
  where active_runs < capacity;
create index run_dispatches_worker_incarnation_state_idx
  on run_dispatches (worker_id, worker_incarnation_id, state, requested_at);
