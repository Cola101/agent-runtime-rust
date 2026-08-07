alter table runtime_workers
  add column accepting_work boolean not null default true,
  add column draining_since timestamptz,
  add column drain_deadline timestamptz,
  add constraint runtime_workers_drain_state_check check (
    (accepting_work and draining_since is null and drain_deadline is null)
    or
    (not accepting_work and draining_since is not null and drain_deadline > draining_since)
  );

alter table runtime_worker_incarnations
  add column accepting_work boolean not null default true,
  add column draining_since timestamptz,
  add column drain_deadline timestamptz,
  add constraint runtime_worker_incarnations_drain_state_check check (
    (accepting_work and draining_since is null and drain_deadline is null)
    or
    (not accepting_work and draining_since is not null and drain_deadline > draining_since)
  );

drop index runtime_worker_incarnations_schedulable_idx;
create index runtime_worker_incarnations_schedulable_idx
  on runtime_worker_incarnations (last_heartbeat desc)
  where accepting_work and active_runs < capacity;
