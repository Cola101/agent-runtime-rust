alter table subagent_calls drop constraint subagent_calls_lifecycle_check;

alter table subagent_calls
  add column child_terminal_event_id uuid,
  add column terminal_status varchar(32),
  add column result jsonb,
  add column result_digest varchar(64),
  add column result_is_error boolean,
  add column delivery_attempt_id uuid,
  add column delivered_event_id uuid,
  add constraint subagent_calls_child_terminal_event_fk
    foreign key (tenant_id, child_terminal_event_id)
    references run_events (tenant_id, event_id),
  add constraint subagent_calls_delivery_dispatch_fk
    foreign key (tenant_id, parent_run_id, delivery_attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  add constraint subagent_calls_delivered_event_fk
    foreign key (tenant_id, delivered_event_id)
    references run_events (tenant_id, event_id),
  add constraint subagent_calls_result_digest_check check (
    result_digest is null or result_digest ~ '^[0-9a-f]{64}$'),
  add constraint subagent_calls_terminal_status_check check (
    terminal_status is null or terminal_status in (
      'succeeded', 'failed', 'cancelled', 'timed_out', 'indeterminate')),
  add constraint subagent_calls_result_size_check check (
    result is null or octet_length(result::text) <= 262144),
  add constraint subagent_calls_lifecycle_check check (
    (state = 'awaiting_checkpoint' and parent_checkpoint_id is null and child_run_id is null
      and child_terminal_event_id is null and result is null and result_digest is null
      and result_is_error is null and delivery_attempt_id is null and delivered_event_id is null)
    or
    (state = 'child_queued' and parent_checkpoint_id is not null and child_run_id is not null
      and child_terminal_event_id is null and result is null and result_digest is null
      and result_is_error is null and delivery_attempt_id is null and delivered_event_id is null)
    or
    (state = 'result_ready' and parent_checkpoint_id is not null and child_run_id is not null
      and child_terminal_event_id is not null and terminal_status is not null
      and result is not null and result_digest is not null and result_is_error is not null
      and delivered_event_id is null)
    or
    (state = 'delivered' and parent_checkpoint_id is not null and child_run_id is not null
      and child_terminal_event_id is not null and terminal_status is not null
      and result is not null and result_digest is not null and result_is_error is not null
      and delivery_attempt_id is not null and delivered_event_id is not null)
    or state = 'cancelled');

create index subagent_calls_result_ready_idx
  on subagent_calls (tenant_id, updated_at, parent_run_id)
  where state = 'result_ready' and delivery_attempt_id is null;
