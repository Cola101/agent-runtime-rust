alter table run_steering_commands
  add column outcome_message_id uuid,
  add column rejection_reason varchar(32),
  add column rejected_at timestamptz;

update run_steering_commands
   set rejection_reason = 'run_terminated', rejected_at = updated_at
 where state = 'rejected';

alter table run_steering_commands
  add constraint run_steering_commands_rejection_check check (
    (state = 'rejected' and rejection_reason is not null and rejected_at is not null)
    or (state <> 'rejected' and rejection_reason is null and rejected_at is null
        and outcome_message_id is null)),
  add constraint run_steering_commands_rejection_reason_check check (
    rejection_reason is null or rejection_reason in (
      'expired','wrong_worker','wrong_worker_incarnation','unknown_attempt',
      'attempt_conflict','lease_expired','attempt_terminal','conflicting_replay',
      'invalid_command','worker_rejected','run_terminated','recovery_indeterminate'));

create unique index run_steering_commands_outcome_message_idx
  on run_steering_commands (tenant_id, outcome_message_id)
  where outcome_message_id is not null;
