# ADR-0050: standalone child cancellation and recovery

## Status

Accepted and implemented for process-crash-safe cancellation intent, model/Tool/MCP
cancellation, in-flight child Checkpoint recovery and durable child-result replay.
Parallel child supervision remains follow-up work.

## Context

ADR-0049 established a serial parent-child-result loop, but the daemon, parent
model call, Tool call and recursively created child Host each owned unrelated
cancellation tokens. The parent Checkpoint did contain the pending spawn, but a
replacement Host treated that state as an incomplete Tool turn and could not
resume it. A child result also had no durable handoff record between child
completion and parent transcript mutation.

Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c` keeps child thread identity and
status independently recoverable and exposes close/wait operations. OpenClaw
`58b4b9430457e91b44f0ccce73ad1b6c6bb11e28` persists child capability and
ownership state, reconciles cancellation races and resumes orphaned children
with an idempotency key. The standalone Runtime needs the same failure ordering
without a database, broker or control plane.

## Decision

1. Each daemon Run owns one root `CancellationToken`. The Host receives that
   token; every child Host receives a child token. Cancellation propagates only
   downward, so a child cannot cancel its parent or sibling.
2. Provider requests and Tool execution contexts receive child tokens from the
   Run root. When cancellation interrupts a child model stream, the child emits
   its own `run.cancelled`; the suspended parent then emits its own terminal
   cancellation instead of accepting a late child result.
3. The parent persists its pending-spawn Checkpoint before starting the child.
   The deterministic delegation ID remains the child Run ID.
4. A child uses its existing Checkpoint when present. The replacement command
   derives an owner epoch strictly greater than the Checkpoint epoch and keeps
   the original root/parent/delegation/depth/role lineage.
5. Child completion is converted into `SubagentResultDelivery` using the durable
   terminal event ID and accumulated model-output events. The result is written
   atomically under the parent Run before parent transcript mutation.
6. On recovery, `WaitForSubagent` resolves in this order: exact persisted result
   receipt; already durable child terminal events; existing child Checkpoint;
   otherwise a fresh child with the deterministic identity. Every result path
   verifies digest, Tool Call, delegation and binding before use.
7. Terminal attempts do not write another ordinary Checkpoint. Their terminal
   event is durable, while the last nonterminal Checkpoint remains the recovery
   boundary; attempting to checkpoint a terminal Worker state is an error.
8. An active daemon writes `Cancelling` atomically before acknowledging the IPC
   request or signalling in-process work. A replacement daemon restores an
   existing Checkpoint with an already-cancelled token, closes the same Kernel
   attempt and never resumes model or Tool work. If the predecessor already
   closed it, recovery leaves the terminal record untouched.
9. The embedded Host binds the Worker attempt cancellation token to the Run's
   root token before MCP discovery. Cancellation therefore closes MCP discovery,
   HTTP/stdio Tool I/O and trusted native process groups. A cancellation error
   from an executor is translated through `WorkerProcessor::cancel`, not through
   the ordinary Tool failure path.
10. Cancellation acknowledgement and final `run.json` persistence share one
    per-Run lifecycle lock. Recovery also scans the durable event log before
    resuming: an already committed Kernel terminal event wins over an older
    `Running` or `Cancelling` local record.

## Consequences

### Positive

- Parent cancellation closes a live child provider connection and produces
  durable terminal events for both child and parent.
- Restart resumes the same child identity without another parent spawn turn.
- The child-complete/parent-not-yet-consumed crash window does not repeat model
  work because the result receipt is independently durable and digest-bound.
- A cancellation acknowledged before a daemon process crash cannot become an
  ordinary recovery or issue another model request.
- Real native Shell, MCP `tools/call` and MCP `initialize` cancellation all
  close their external resource and emit one `run.cancelled` terminal event.
- A crash after `run.succeeded` but before the local lifecycle record update is
  reconciled as succeeded and cannot append a contradictory cancellation.
- The implementation remains filesystem-only and adds no external dependency.

### Negative

- A cancellation recovered without any Checkpoint can preserve the terminal
  local Run record but cannot reconstruct the missing Kernel attempt identity,
  so it has no synthetic `run.cancelled` event. Power-loss durability also
  remains bounded by the local filesystem; this ADR proves process crashes.
- A serial recursive Host is not the final supervisor needed for eight active
  children, wait/send/close or nested approval routing.

## Alternatives Considered

- **Restart the parent model and ask it to spawn again:** rejected because model
  output is nondeterministic and could duplicate child side effects.
- **Use child event logs only:** rejected because the result handoff must survive
  the exact window after child completion but before parent mutation.
- **Add PostgreSQL or NATS locally:** rejected because cancellation and recovery
  are Kernel/Host responsibilities and standalone execution is a hard constraint.

## References

- ADR-0049: standalone role subagent execution
- `runtime/apps/runtime-host/tests/subagent_cancellation.rs`
- `runtime/apps/runtime-host/tests/daemon_recovery.rs`
- `runtime/apps/runtime-host/tests/execution_cancellation.rs`
