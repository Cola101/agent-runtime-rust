# Bounded parallel subagent evidence — 2026-08-09

## Scope

This evidence covers one standalone parent with an adjacent two-child batch,
including real simultaneous HTTP model requests, deterministic result binding,
usage settlement, parent cancellation, partial-batch crash recovery and one
child terminal failure. It does not claim Codex/OpenClaw-equivalent long-lived
agent messaging, tenant fairness or complete duration enforcement.

## Behavioural proof

- `two_subagents_are_inflight_before_either_child_completes` holds the first
  child response until the provider accepts the second. The old serial Host
  failed this condition; the current Host accepts both, completes them in
  reverse order, then gives the parent both original Tool IDs and contents.
- Both children report 150 Tokens. Their digest-bound result receipts settle
  exactly 300 Tokens into the parent Checkpoint.
- `cancelling_the_parent_closes_every_inflight_child_in_the_batch` starts two
  live SSE streams, cancels the root token and observes both TCP connections
  close. The parent emits exactly one `run.cancelled`.
- `recovery_reuses_completed_batch_receipts_and_restarts_only_unfinished_children`
  crashes the first Host after one atomic child receipt exists while its sibling
  is streaming. The replacement loads the completed receipt, invokes only the
  unfinished child, supplies both results to the parent and finishes with two
  receipts and no completed-child replay.
- `one_failed_child_is_bound_as_an_error_without_losing_its_successful_sibling`
  gives one child a normal terminal response and one a content-filter failure.
  The parent receives both; the failure includes `terminal_status=failed` and
  `is_error=true`, and the parent can still complete successfully.
- Worker tests reject cumulative Token/cost reservations and the ninth child
  before consuming the Tool call. Actual child usage remains charged after the
  reservation is released. Replaying the same result returns the same receipt
  without double charging.
- Protocol tests prove non-zero usage is digest-bound and that legacy zero-usage
  result receipts still verify.

## Source comparison used

- Codex `ff352fab6209`: multi-agent spawn returns an Agent ID immediately;
  agent-registry slots bound spawn/residency, and wait/send/close are separate
  lifecycle operations.
- OpenClaw `58b4b9430457`: swarm scheduling uses persistent registrations, FIFO
  group reservations, layered concurrency caps, settled-child collection and
  orphan recovery.
- This implementation adopts bounded admission, deterministic collection and
  durable recovery, but retains blocking Tool semantics and filesystem-only
  authority for the current independent Rust Kernel target.

## Validation

- Targeted concurrency suite: 6 passed, 0 failed, including simultaneous
  execution, batch cancellation, partial crash recovery, one-child failure and
  approval ordering plus the shared parent duration deadline.
- Worker assignment suite: 51 passed, 0 failed, including cumulative budget,
  actual usage idempotency and the eight-child admission cap.
- Full workspace: 401 passed, 0 failed, 5 explicitly ignored live integration
  cases; 406 test items listed.
- Workspace Clippy with all targets/features under `-D warnings`, formatting and
  `git diff --check`: passed.
- Residue audit found no Runtime/Tool process, partial file, Host/Tool temporary
  directory or Unix socket. The 27 GiB reusable Rust debug `target` cache was
  retained and was not treated as a shipped runtime artifact.
