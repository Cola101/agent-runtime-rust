alter table run_dispatches drop constraint run_dispatches_pkey;
alter table run_dispatches drop constraint run_dispatches_acceptance_check;

alter table run_dispatches
  add constraint run_dispatches_pkey primary key (tenant_id, run_id, attempt_id);

alter table run_dispatches
  add constraint run_dispatches_acceptance_check check (
    (state = 'requested' and accepted_at is null) or
    (state in ('accepted', 'finished') and accepted_at is not null) or
    state = 'lost');

create unique index run_dispatches_one_active_attempt_idx
  on run_dispatches (tenant_id, run_id)
  where state in ('requested', 'accepted');

create index run_dispatches_expired_active_idx
  on run_dispatches (lease_expires_at)
  where state in ('requested', 'accepted');
