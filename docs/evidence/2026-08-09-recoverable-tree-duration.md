# Recoverable tree duration evidence — 2026-08-09

## Behavioural proof

- The initial RED, `parent_duration_budget_stops_a_streaming_child_and_terminalizes_the_tree`,
  held a child SSE stream open with heartbeats. The old Runtime exceeded the
  2.5-second guard; the implementation now closes the stream at the one-second
  parent budget and emits one parent `run.timed_out`.
- `duration_budget_reaps_a_running_native_tool_and_times_out_the_run` starts a
  real `shell.exec` process, observes its PID, expires the Run and proves the
  process group no longer exists.
- The MCP duration tests hold real HTTP initialize and `tools/call` sockets
  open. Expiry closes both; discovery never reaches the model and the Tool Run
  terminates as timed out.
- `approval_wait_does_not_consume_the_run_duration_budget` proves the parked
  Checkpoint has `execution_time.active=false`, sleeps longer than the entire
  two-second Run budget, approves, executes the real workspace Tool and
  succeeds.
- `recovery_charges_an_active_crash_gap_and_times_out_without_reinvoking_the_model`
  crashes a separate Tokio runtime during a live model call, waits past the
  one-second budget, restores, emits one timeout and proves the provider still
  saw exactly one request.
- Worker assignment tests prove active attempts become duration terminals and
  that original and rebound approvals persist a stopped clock and resume it
  only after a decision.

## Reference comparison

- Codex `ff352fab6209`: `wait_agent` uses `tokio::time::Instant` and
  `timeout_at`; task and Tool cancellation are mature. No inspected structure
  persists a parent/child execution-duration balance across restart.
- OpenClaw `58b4b9430457`: `subagent-run-timeout.ts` derives a persisted
  absolute deadline; the registry converts post-deadline completion to timeout
  and carries accumulated session runtime through replacement. It does not
  provide one approval-paused active-time budget shared by the parent tree.
- This Runtime is stronger on the narrow duration invariant. It remains behind
  both projects on long-lived child interaction, history/context lifecycle and
  operational maturity.

## Evidence boundary

The standalone paths use real HTTP sockets, filesystem Checkpoints and native
processes. Provider responses are deterministic local fixtures, so these tests
prove Runtime semantics, not vendor model quality. The optional NATS Worker
deadline publisher compiled and its protocol-neutral core is tested, but no
external NATS service was started in this stage.

## Validation

- Runtime workspace: 401 passed, 0 failed, 5 explicitly ignored external live
  cases; 406 test items listed.
- Clippy: workspace/all targets/all features with `-D warnings` passed.
- Rust formatting and `git diff --check` passed.
