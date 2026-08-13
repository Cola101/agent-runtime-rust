# ADR-0087: Interruption preserves started Tool uncertainty

## Status

Accepted and implemented in the Worker core and standalone Rust Host. The
optional NATS transport shares the Worker classification but was not started
in this local-only stage.

## Context

Cancellation and duration expiry already closed native process trees and live
MCP I/O. They then unconditionally recorded `run.cancelled` or
`run.timed_out`. That terminal described the caller's interruption intent but
could be misread as proof that an already-started external side effect did not
happen.

The Runtime already persists `tool.execution.started` before execution and
freezes each Tool's effect. Losing that evidence on the interruption path was
inconsistent with live executor-failure and Host-recovery semantics.

## Decision

1. Cancellation and duration expiry inspect the durable started Tool boundary
   before selecting a terminal state.
2. If an outstanding started Tool is `NonIdempotent` or `Unknown`, the Run
   terminates as `run.indeterminate`, never as cancelled or timed out.
3. The indeterminate event retains the existing Tool call, binding, sandbox,
   source attempt and started-event evidence. It additionally records
   `interrupted_by=cancellation|duration_timeout` and the caller-requested
   `cancelled|timed_out` status.
4. A started `Pure` or `Idempotent` Tool, or an interruption before any unsafe
   Tool start, keeps the ordinary cancelled/timed-out terminal.
5. Resource cancellation still happens immediately. `indeterminate` does not
   mean the interruption failed; it means interruption cannot prove whether
   the external effect happened before the resource closed.
6. The standalone Host persists the indeterminate Checkpoint before exposing
   the terminal event, preserving the existing operator-reconciliation input.
7. Unsafe Tools remain serial barriers, so one Run cannot contain multiple
   simultaneously started unsafe Tool calls under the current execution
   policy.

## Consequences

### Positive

- User intent and side-effect certainty are no longer collapsed into one
  misleading terminal label.
- Cancellation, timeout, executor failure and replacement recovery use the
  same frozen-effect boundary.
- Real process/MCP cleanup remains bounded while the source Run stays immutable
  and non-replayable.
- A durable terminal Checkpoint and event carry enough evidence for operator
  reconciliation.

### Negative and incomplete

- Callers must treat an accepted cancellation command as an intent
  acknowledgement, not a promise that the final Run status is `cancelled`.
- MCP protocol-level cancellation notifications and progress tokens are not
  implemented; HTTP closure or stdio process/session teardown remains the
  current transport action.
- The optional NATS path was compile- and workspace-tested but not exercised
  against a live broker in this stage.
- Real Linux cgroup enforcement remains disabled pending live gates.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/core/src/tools/parallel.rs` and
  `codex-rs/core/src/tools/handlers/mcp.rs`
- OpenClaw revision `58b4b9430457`,
  `src/agents/embedded-agent-runner/{replay-state,run/terminal-timeout}.ts`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/tests/execution_cancellation.rs`
- `docs/evidence/2026-08-11-interrupted-tool-uncertainty.md`
