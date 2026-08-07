alter table tool_executions
  drop constraint tool_executions_sandbox_check;

alter table tool_executions
  add constraint tool_executions_sandbox_check
    check (sandbox in ('restricted_container', 'kata', 'trusted_native'));
