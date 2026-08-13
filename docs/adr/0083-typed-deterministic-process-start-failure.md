# ADR-0083: Typed deterministic process-start failure propagation

## Status

Accepted and implemented. The production Linux cgroup backend remains
fail-closed pending live Linux gates.

## Context

ADR-0082 made a synchronous process spawn failure durable as
`Terminated/start_failed`, but `ProcessSessionToolExecutor` converted every
Manager error to `PersistentProcessSession(String)`. The Worker therefore
published a generic `tool_execution_failed`, while the standalone Host aborted
the Agent Loop and exposed the private operating-system reason through its
operator error.

Blindly converting every Tool execution error into a normal Tool Result would
be unsafe. Once a non-idempotent or unknown Tool may have crossed an external
side-effect boundary, an error is not proof that the effect did not happen.

## Decision

1. `ProcessSessionToolExecutor` maps `ProcessSessionError::StartFailed` to the
   typed `ToolExecutionError::ProcessSessionStartFailed`, retaining Session ID
   and the private operating-system reason for operator diagnostics.
2. `ToolExecutionError::deterministic_failure_result` is deliberately partial.
   It returns a Tool Result only for failures that prove execution never crossed
   the external side-effect boundary.
3. The public result contains stable code `process_session_start_failed`, a
   fixed safe message, and the Session ID. It never contains the private reason.
4. The Worker uses the same safe result as the content of its durable
   `tool.result` event. The standalone Host feeds that result back into the next
   model turn instead of terminating the Run.
5. Other Tool errors keep their existing recovery behavior. In particular,
   this ADR does not authorize treating unclassified non-idempotent or unknown
   failures as completed.
6. The Manager boundary is tested with a real synchronous OS spawn failure.
   The Host boundary uses the exact typed ToolExecutor result in a real
   loopback HTTP Agent Loop so each boundary is deterministic and independently
   observable.

## Consequences

### Positive

- Durable launch truth, Tool error identity, Worker event content and model
  feedback now agree on one Session ID.
- The model receives an actionable stable code without filesystem paths or OS
  diagnostics.
- A known pre-side-effect failure can be repaired by a later model turn without
  making the Run indeterminate or silently retrying the process.
- The partial conversion API makes the certainty boundary explicit instead of
  relying on callers to infer it from error strings.

### Negative and incomplete

- The private reason still exists in the typed Rust error and may appear in
  operator-only diagnostics; callers must use the safe result for model/event
  surfaces.
- The cloud Worker still needs a broader effect-aware audit for other
  `ToolExecutionError` variants. Generic failure of a NonIdempotent/Unknown Tool
  must not be recorded as completed when its side effect is uncertain.
- Real Linux cgroup enforcement, PTY and Windows process supervision remain
  incomplete.

## References

- Codex revision `ff352fab6209`, `codex-rs/core/src/{exec,spawn}.rs`
- OpenClaw revision `58b4b9430457`,
  `src/process/supervisor/supervisor.ts` and
  `packages/agent-core/src/agent-loop.ts`
- `runtime/crates/tool-runtime/src/{lib,process_session}.rs`
- `runtime/apps/{worker,runtime-host}/src/lib.rs`
- `docs/evidence/2026-08-11-typed-deterministic-process-start-failure.md`
