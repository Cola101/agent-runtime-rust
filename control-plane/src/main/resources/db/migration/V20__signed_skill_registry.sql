alter table agent_versions add column application_id uuid;

update agent_versions av
   set application_id = p.application_id
  from agents a
  join workspaces w on w.tenant_id = a.tenant_id and w.id = a.workspace_id
  join projects p on p.tenant_id = w.tenant_id and p.id = w.project_id
 where a.tenant_id = av.tenant_id and a.id = av.agent_id;

alter table agent_versions alter column application_id set not null;
alter table agent_versions add constraint agent_versions_application_fk
  foreign key (tenant_id, application_id) references applications (tenant_id, id);
alter table agent_versions add constraint agent_versions_application_id_unique
  unique (tenant_id, application_id, id);

create table skills (
  tenant_id uuid not null,
  id uuid not null,
  application_id uuid not null,
  name varchar(120) not null,
  state varchar(20) not null default 'active',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint skills_application_fk foreign key (tenant_id, application_id)
    references applications (tenant_id, id),
  constraint skills_application_id_unique unique (tenant_id, application_id, id),
  constraint skills_name_unique unique (tenant_id, application_id, name),
  constraint skills_state_check check (state in ('active', 'disabled'))
);

create table skill_versions (
  tenant_id uuid not null,
  id uuid not null,
  application_id uuid not null,
  skill_id uuid not null,
  semantic_version varchar(64) not null,
  artifact jsonb not null,
  artifact_digest char(64) not null,
  signing_key_id varchar(128) not null,
  signature text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint skill_versions_skill_fk foreign key (tenant_id, application_id, skill_id)
    references skills (tenant_id, application_id, id),
  constraint skill_versions_application_id_unique unique (tenant_id, application_id, id),
  constraint skill_versions_semantic_version_unique
    unique (tenant_id, application_id, skill_id, semantic_version),
  constraint skill_versions_artifact_object_check check (jsonb_typeof(artifact) = 'object'),
  constraint skill_versions_artifact_digest_check check (artifact_digest ~ '^[0-9a-f]{64}$'),
  constraint skill_versions_signature_check check (length(signature) between 80 and 128)
);

create table agent_version_skills (
  tenant_id uuid not null,
  application_id uuid not null,
  agent_version_id uuid not null,
  ordinal smallint not null,
  skill_version_id uuid not null,
  artifact_digest char(64) not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, agent_version_id, ordinal),
  constraint agent_version_skills_skill_unique
    unique (tenant_id, agent_version_id, skill_version_id),
  constraint agent_version_skills_ordinal_check check (ordinal between 0 and 15),
  constraint agent_version_skills_agent_version_fk
    foreign key (tenant_id, application_id, agent_version_id)
    references agent_versions (tenant_id, application_id, id) on delete cascade,
  constraint agent_version_skills_skill_version_fk
    foreign key (tenant_id, application_id, skill_version_id)
    references skill_versions (tenant_id, application_id, id),
  constraint agent_version_skills_artifact_digest_check check (
    artifact_digest ~ '^[0-9a-f]{64}$')
);

create index skills_application_idx
  on skills (tenant_id, application_id, state, id);
create index skill_versions_skill_idx
  on skill_versions (tenant_id, skill_id, created_at, id);
create index agent_version_skills_skill_idx
  on agent_version_skills (tenant_id, skill_version_id, agent_version_id);

alter table skills enable row level security;
alter table skills force row level security;
create policy tenant_isolation on skills
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());

alter table skill_versions enable row level security;
alter table skill_versions force row level security;
create policy tenant_isolation on skill_versions
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());

alter table agent_version_skills enable row level security;
alter table agent_version_skills force row level security;
create policy tenant_isolation on agent_version_skills
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
