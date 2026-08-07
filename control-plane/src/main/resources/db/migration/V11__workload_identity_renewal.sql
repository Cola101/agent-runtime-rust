alter table run_dispatches
  add column workload_identity_generation bigint not null default 1,
  add column workload_identity_expires_at timestamptz default now();

update run_dispatches
   set workload_identity_expires_at = lease_expires_at;

alter table run_dispatches
  alter column workload_identity_expires_at set not null,
  alter column workload_identity_expires_at drop default,
  add constraint run_dispatches_identity_generation_check
    check (workload_identity_generation > 0),
  add constraint run_dispatches_identity_expiry_check
    check (workload_identity_expires_at <= lease_expires_at);
