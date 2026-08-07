create table model_policies (
  tenant_id uuid not null,
  id uuid not null,
  workspace_id uuid not null,
  name varchar(200) not null,
  policy jsonb not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint model_policies_workspace_fk foreign key (tenant_id, workspace_id)
    references workspaces (tenant_id, id),
  constraint model_policies_workspace_id_unique unique (tenant_id, workspace_id, id),
  constraint model_policies_name_unique unique (tenant_id, workspace_id, name),
  constraint model_policies_policy_object_check check (jsonb_typeof(policy) = 'object')
);

alter table runs add column model_policy_id uuid not null;
alter table runs add constraint runs_model_policy_workspace_fk
  foreign key (tenant_id, workspace_id, model_policy_id)
  references model_policies (tenant_id, workspace_id, id);

create index runs_model_policy_idx on runs (tenant_id, model_policy_id, created_at desc);

alter table model_policies enable row level security;
alter table model_policies force row level security;
create policy tenant_isolation on model_policies
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
