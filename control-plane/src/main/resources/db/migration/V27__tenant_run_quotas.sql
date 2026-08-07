-- Per-tenant concurrency quota.
--
-- Until now a tenant could create Runs without limit. The only concurrency
-- bound anywhere was MAX_ACTIVE_CHILDREN in subagent admission, which protects a
-- parent from its own children and says nothing about one tenant consuming every
-- Worker slot the platform has.
--
-- The row exists to be locked. Counting active Runs and then inserting is not
-- enough on its own: two requests can both count under the limit and both
-- insert. Taking the tenant's quota row `for update` first serialises admission
-- per tenant, which is the same shape subagent admission already uses on the
-- parent Run row.

create table tenant_run_quotas (
  tenant_id uuid not null primary key,
  max_active_runs integer not null,
  updated_at timestamptz not null default now(),
  constraint tenant_run_quotas_active_check check (max_active_runs > 0)
);

comment on table tenant_run_quotas is
  'Per-tenant admission limits. Absent means the default applies; the row is also the lock admission serialises on.';

-- Existing tenants get the default so behaviour does not change silently for
-- anyone already running: the limit only becomes visible when it is reached.
insert into tenant_run_quotas (tenant_id, max_active_runs)
select distinct tenant_id, 64 from runs
on conflict (tenant_id) do nothing;
