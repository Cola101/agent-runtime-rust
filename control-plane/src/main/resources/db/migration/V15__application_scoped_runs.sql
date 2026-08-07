alter table runs add column application_id uuid;

update runs r
   set application_id = p.application_id
  from workspaces w
  join projects p
    on p.tenant_id = w.tenant_id and p.id = w.project_id
 where w.tenant_id = r.tenant_id and w.id = r.workspace_id;

alter table runs alter column application_id set not null;

alter table runs add constraint runs_application_fk
  foreign key (tenant_id, application_id)
  references applications (tenant_id, id);

alter table runs drop constraint runs_idempotency_unique;
alter table runs add constraint runs_application_idempotency_unique
  unique (tenant_id, application_id, idempotency_key);

create index runs_application_created_idx
  on runs (tenant_id, application_id, created_at desc);
