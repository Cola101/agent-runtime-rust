alter table sessions add column title varchar(200);

alter table workspaces add constraint workspaces_project_name_unique
  unique (tenant_id, project_id, name);

alter table agents add constraint agents_workspace_name_unique
  unique (tenant_id, workspace_id, name);
