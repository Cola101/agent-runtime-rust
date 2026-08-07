create table model_providers (
  tenant_id uuid not null,
  id uuid not null,
  application_id uuid not null,
  name varchar(200) not null,
  protocol varchar(40) not null,
  endpoint varchar(2048) not null,
  model varchar(200) not null,
  credential_envelope jsonb not null,
  state varchar(20) not null default 'active',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint model_providers_application_fk foreign key (tenant_id, application_id)
    references applications (tenant_id, id),
  constraint model_providers_application_id_unique unique (tenant_id, application_id, id),
  constraint model_providers_name_unique unique (tenant_id, application_id, name),
  constraint model_providers_protocol_check check (
    protocol in ('openai_compatible', 'openai_responses', 'anthropic_messages')),
  constraint model_providers_state_check check (state in ('active', 'disabled')),
  constraint model_providers_credential_object_check check (
    jsonb_typeof(credential_envelope) = 'object')
);

alter table model_policies add column application_id uuid;
update model_policies mp
   set application_id = p.application_id
  from workspaces w
  join projects p on p.tenant_id = w.tenant_id and p.id = w.project_id
 where w.tenant_id = mp.tenant_id and w.id = mp.workspace_id;
alter table model_policies alter column application_id set not null;
alter table model_policies add constraint model_policies_application_fk
  foreign key (tenant_id, application_id) references applications (tenant_id, id);
alter table model_policies add constraint model_policies_application_id_unique
  unique (tenant_id, application_id, id);

create table model_policy_candidates (
  tenant_id uuid not null,
  application_id uuid not null,
  model_policy_id uuid not null,
  provider_id uuid not null,
  priority smallint not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, model_policy_id, priority),
  constraint model_policy_candidates_provider_unique
    unique (tenant_id, model_policy_id, provider_id),
  constraint model_policy_candidates_priority_check check (priority between 0 and 7),
  constraint model_policy_candidates_policy_fk
    foreign key (tenant_id, application_id, model_policy_id)
    references model_policies (tenant_id, application_id, id) on delete cascade,
  constraint model_policy_candidates_provider_fk
    foreign key (tenant_id, application_id, provider_id)
    references model_providers (tenant_id, application_id, id)
);

create index model_providers_application_idx
  on model_providers (tenant_id, application_id, state, id);
create index model_policy_candidates_provider_idx
  on model_policy_candidates (tenant_id, provider_id, model_policy_id);

alter table model_providers enable row level security;
alter table model_providers force row level security;
create policy tenant_isolation on model_providers
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());

alter table model_policy_candidates enable row level security;
alter table model_policy_candidates force row level security;
create policy tenant_isolation on model_policy_candidates
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
