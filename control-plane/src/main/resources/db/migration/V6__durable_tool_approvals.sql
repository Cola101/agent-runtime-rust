alter table approvals
  add column attempt_id uuid,
  add column worker_id uuid,
  add column tool_call_id varchar(256),
  add column binding_digest varchar(64);

alter table approvals
  add constraint approvals_dispatch_fk
    foreign key (tenant_id, run_id, attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  add constraint approvals_worker_fk
    foreign key (worker_id) references runtime_workers (id),
  add constraint approvals_call_unique
    unique (tenant_id, run_id, attempt_id, tool_call_id),
  add constraint approvals_tool_call_check
    check (tool_call_id is null or (length(btrim(tool_call_id)) > 0 and length(tool_call_id) <= 256)),
  add constraint approvals_binding_digest_check
    check (binding_digest is null or binding_digest ~ '^[0-9a-f]{64}$');

-- The columns are introduced nullable so an upgrade can preserve historical approvals.
-- Every new runtime approval is written with all four immutable binding fields.
