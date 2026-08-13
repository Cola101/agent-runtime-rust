# ADR-0053: recoverable tree execution duration budget

## Status

Accepted and implemented in the protocol-neutral Worker core and standalone
Rust Host. The standalone model, native Tool, MCP and subagent boundaries are
behaviourally verified. The optional NATS Worker publisher is wired and covered
by core tests, but was not exercised against an external NATS service in this
stage.

## Context

`RunBudget.max_duration_seconds` was previously validation-only. Provider,
Tool, MCP discovery and child waits each had an operation timeout, but none
could stop the whole Run or its child tree. A streaming child could therefore
keep a one-second parent alive until the provider's 60-second idle timeout.

Codex `ff352fab6209` uses monotonic deadlines for individual operations such as
`wait_agent` and propagates cancellation through Tool/agent tasks, but the
inspected code has no persisted parent/child duration ledger. OpenClaw
`58b4b9430457` persists an absolute per-subagent deadline, normalizes late
success after that deadline to timeout, and carries accumulated session runtime
across steer/restart. Its deadline is wall-clock and per run rather than a
shared parent-tree active-time budget.

The product contract says approval wait does not consume execution time. A
single absolute wall-clock deadline is therefore insufficient, while an
in-memory `Instant` alone cannot survive a process crash.

## Decision

1. Every accepted attempt owns an execution clock. Active slices use
   `std::time::Instant`; wall time is not consulted during normal execution.
2. Worker Checkpoint schema 12 stores elapsed active milliseconds, the UTC
   checkpoint instant and whether the clock was active. The complete structure
   is covered by the Checkpoint digest.
3. Restoring an active Checkpoint conservatively charges the interval from the
   Checkpoint to restore. Restoring a parked approval does not charge that gap.
   A backwards UTC jump fails closed by exhausting the duration rather than
   granting extra time.
4. Planning an approval stops the clock before its Checkpoint is published.
   Applying a decision starts a new monotonic slice. Recovery may charge the
   work required to restore authority/catalogs, then pauses again when the
   approval is rebound.
5. Duration expiry cancels the same downward token used by models, native
   Tools, MCP and subagents, then emits exactly one `run.timed_out` with
   `kind=duration_budget_exhausted`. It is distinct from operator cancellation.
6. The parent Host clock remains active while a child batch runs. That exact
   parent deadline therefore bounds the whole tree without adding concurrent
   child durations. Each child request is also clamped to the rounded-up parent
   remainder and retains its own smaller cap.
7. Active crash downtime is charged because the Runtime cannot prove useful
   work stopped at the last active Checkpoint. This is intentionally
   conservative; approval parking is the explicit non-charging state.
8. The optional NATS Worker checks active clocks, drops unpublished attempt
   updates, cancels supervisors, publishes the timeout terminal and releases
   capacity only after PubAck. Initial and recovery MCP discovery are directly
   bounded by the remaining Run duration so a blocked discovery call cannot
   prevent enforcement.
9. Legacy Checkpoints without execution timing remain readable and start a new
   clock on restore. New Checkpoints always write schema 12 timing state.

## Consequences

### Positive

- One budget now covers model streaming, Tool processes, MCP discovery/calls
  and the complete standalone child tree.
- Human approval latency is excluded durably across daemon restart.
- A hard crash cannot reset or extend an active Run's duration.
- Timeout classification and cancellation resource cleanup share one path.

### Negative

- Active crash downtime counts against the Run even though the process was not
  computing; this is the safe choice without an external monotonic authority.
- Child duration is represented in whole seconds, so the child command uses a
  ceiling while the parent watchdog retains the exact sub-second remainder.
- NATS publication latency is bounded by the Worker poll cadence after resource
  cancellation. This stage has no external NATS behavioural evidence.

## References

- ADR-0050: standalone child cancellation and recovery
- ADR-0052: bounded parallel subagent supervision
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/tests/execution_cancellation.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
- `runtime/apps/runtime-host/tests/approval_flow.rs`
- `runtime/apps/runtime-host/tests/daemon_recovery.rs`
