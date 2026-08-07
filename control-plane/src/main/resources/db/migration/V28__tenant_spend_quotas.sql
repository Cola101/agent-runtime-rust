-- Per-tenant spend limits, over a rolling window.
--
-- Concurrency (V27) stops one tenant taking every Worker slot. It does nothing
-- about cost: a tenant staying inside two concurrent Runs can spend without
-- bound, one Run after another.
--
-- No new usage ledger. Consumption is already durable in run_events as
-- `model.usage` payloads carrying input_tokens, output_tokens and cost_micros,
-- and the Worker writes those as part of the same event stream everything else
-- is reconciled from. A second table would be a second truth, and the two would
-- drift the first time an event was replayed or a write was lost.

alter table tenant_run_quotas
  add column max_cost_micros_per_day bigint,
  add column max_tokens_per_day bigint;

comment on column tenant_run_quotas.max_cost_micros_per_day is
  'Rolling 24h spend ceiling in micros. Null means unlimited, which is what every existing tenant gets so this changes no behaviour on deploy.';
comment on column tenant_run_quotas.max_tokens_per_day is
  'Rolling 24h token ceiling. Null means unlimited.';

alter table tenant_run_quotas
  add constraint tenant_run_quotas_cost_check
    check (max_cost_micros_per_day is null or max_cost_micros_per_day > 0),
  add constraint tenant_run_quotas_tokens_check
    check (max_tokens_per_day is null or max_tokens_per_day > 0);

-- Aggregating a day of events per admission would scan the whole window every
-- time. The index keeps that to the rows that matter.
create index if not exists run_events_tenant_usage_idx
  on run_events (tenant_id, occurred_at)
  where type = 'model.usage';
