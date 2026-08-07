create index run_steering_commands_run_requested_idx
  on run_steering_commands (tenant_id, run_id, requested_at desc);
