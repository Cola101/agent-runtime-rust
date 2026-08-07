-- Per-tenant dispatch weight, for fair sharing of the outbox drain.
--
-- Until now the drain was strict global FIFO: `order by created_at, id` across
-- every tenant. One tenant enqueuing a burst fills every batch until it is
-- drained, and every other tenant waits behind it however small their work is.
-- Quotas cannot fix this. A quota refuses admission; it says nothing about the
-- order of what was already admitted.
--
-- The weight lives on tenant_run_quotas rather than in a table of its own. That
-- row is already the per-tenant admission knob and is already locked during
-- admission, so a separate table would add a second place to look for "what
-- limits apply to this tenant" without adding anything else.

alter table tenant_run_quotas
  add column dispatch_weight integer not null default 1;

comment on column tenant_run_quotas.dispatch_weight is
  'Relative share of each outbox drain batch. A tenant at weight 4 is served four messages for every one at weight 1. Default 1 means equal shares, which is what every existing tenant gets.';

alter table tenant_run_quotas
  add constraint tenant_run_quotas_weight_check
    check (dispatch_weight > 0);

-- The drain ranks unpublished messages per tenant before it orders them, so it
-- reads every unpublished row each poll. Without this it is a sequential scan of
-- the whole table including the published history, which grows without bound.
create index if not exists outbox_events_unpublished_idx
  on outbox_events (tenant_id, created_at, id)
  where published_at is null;
