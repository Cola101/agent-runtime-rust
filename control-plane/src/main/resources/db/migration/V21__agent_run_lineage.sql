alter table runs
  add column root_run_id uuid,
  add column parent_run_id uuid,
  add column delegation_id uuid,
  add column subagent_depth smallint not null default 0,
  add column agent_role varchar(80) not null default 'primary';

alter table runs
  add constraint runs_root_run_fk foreign key (tenant_id, root_run_id)
    references runs (tenant_id, id),
  add constraint runs_parent_run_fk foreign key (tenant_id, parent_run_id)
    references runs (tenant_id, id),
  add constraint runs_agent_lineage_check check (
    (subagent_depth = 0 and root_run_id is null and parent_run_id is null
      and delegation_id is null and agent_role = 'primary')
    or
    (subagent_depth between 1 and 3 and root_run_id is not null and parent_run_id is not null
      and delegation_id is not null and agent_role <> 'primary'
      and root_run_id <> id and parent_run_id <> id)
  );

create unique index runs_delegation_unique
  on runs (tenant_id, delegation_id)
  where delegation_id is not null;
create index runs_root_depth_created_idx
  on runs (tenant_id, root_run_id, subagent_depth, created_at)
  where root_run_id is not null;
create index runs_parent_active_idx
  on runs (tenant_id, parent_run_id, created_at)
  where parent_run_id is not null
    and status in ('queued', 'running', 'waiting_approval', 'suspended');
