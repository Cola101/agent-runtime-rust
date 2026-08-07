create table recovery_metric_buckets (
  last_confirmed_healthy_at timestamptz primary key,
  waiting_capacity bigint not null default 0,
  recovery_requested bigint not null default 0,
  updated_at timestamptz not null default clock_timestamp(),
  constraint recovery_metric_buckets_counts_check check (
    waiting_capacity >= 0 and recovery_requested >= 0)
);

comment on table recovery_metric_buckets is
  'Tenant-free transactional rollup for low-cardinality platform recovery metrics';

insert into recovery_metric_buckets (
  last_confirmed_healthy_at,waiting_capacity,recovery_requested)
select last_confirmed_healthy_at,
       count(*) filter (where state = 'waiting_capacity'),
       count(*) filter (where state = 'recovery_requested')
  from recovery_incidents
 where resolved_at is null
 group by last_confirmed_healthy_at;

create function maintain_recovery_metric_buckets()
returns trigger
language plpgsql
as $$
begin
  if tg_op <> 'INSERT' and old.resolved_at is null
      and old.state in ('waiting_capacity', 'recovery_requested') then
    update recovery_metric_buckets
       set waiting_capacity = waiting_capacity
             - case when old.state = 'waiting_capacity' then 1 else 0 end,
           recovery_requested = recovery_requested
             - case when old.state = 'recovery_requested' then 1 else 0 end,
           updated_at = clock_timestamp()
     where last_confirmed_healthy_at = old.last_confirmed_healthy_at
       and (old.state <> 'waiting_capacity' or waiting_capacity > 0)
       and (old.state <> 'recovery_requested' or recovery_requested > 0);
    if not found then
      raise exception 'recovery metric rollup is inconsistent for incident %', old.incident_id;
    end if;
    delete from recovery_metric_buckets
     where last_confirmed_healthy_at = old.last_confirmed_healthy_at
       and waiting_capacity = 0 and recovery_requested = 0;
  end if;

  if tg_op <> 'DELETE' and new.resolved_at is null
      and new.state in ('waiting_capacity', 'recovery_requested') then
    insert into recovery_metric_buckets (
      last_confirmed_healthy_at,waiting_capacity,recovery_requested)
    values (
      new.last_confirmed_healthy_at,
      case when new.state = 'waiting_capacity' then 1 else 0 end,
      case when new.state = 'recovery_requested' then 1 else 0 end)
    on conflict (last_confirmed_healthy_at) do update
       set waiting_capacity = recovery_metric_buckets.waiting_capacity
             + excluded.waiting_capacity,
           recovery_requested = recovery_metric_buckets.recovery_requested
             + excluded.recovery_requested,
           updated_at = clock_timestamp();
  end if;

  if tg_op = 'DELETE' then
    return old;
  end if;
  return new;
end;
$$;

revoke all on function maintain_recovery_metric_buckets() from public;

create trigger recovery_incident_metric_rollup
after insert or update or delete on recovery_incidents
for each row execute function maintain_recovery_metric_buckets();
