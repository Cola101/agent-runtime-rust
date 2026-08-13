# ADR-0084: Effect-aware live Tool execution failure

## Status

Accepted and implemented for the Worker core and standalone Rust Host. The
optional NATS transport is compile-tested but was not started in this local
stage. Production Linux cgroup enforcement remains disabled pending live gates.

## Context

The Runtime already recovered a started `NonIdempotent` or `Unknown` Tool as
`run.indeterminate` after a Host replacement. The live executor-error path did
not preserve the same rule:

- the cloud Worker converted every `ToolExecutionError` into a completed error
  `tool.result`, even when the external effect might already have happened;
- the standalone Host returned `LocalRuntimeError::ToolExecution`, leaving the
  durable started boundary for a later replacement to classify.

The same Tool could therefore receive different certainty semantics depending
on whether the Host crashed or merely received an executor error.

## Decision

1. `WorkerProcessor::record_tool_execution_failure` validates the attempt,
   Tool call, binding digest and durable started boundary before classification.
2. A failure accepted by `deterministic_failure_result` becomes a redacted
   model-visible Tool Result for every effect class because execution is proven
   not to have crossed the external side-effect boundary.
3. Other failures of `Pure` and `Idempotent` Tools become redacted error Tool
   Results. They are safe to expose as completed outcomes under their frozen
   effect contract; this decision does not require automatic retry.
4. Other failures of `NonIdempotent` and `Unknown` Tools retain the outstanding
   request and started event and terminate as bound `run.indeterminate` evidence.
   They never trigger the next model turn or automatic Tool replay.
5. The standalone Host persists the resulting Tool Result or indeterminate
   Checkpoint before returning. Operator reconciliation creates a separate Run
   with explicit applied/not-applied evidence; it never mutates the source Run.
6. The NATS Worker queues the indeterminate event through the same retryable
   publication path, then acknowledges the terminal only after publication.
   It classifies the queued event itself so a stale non-terminal Tool Result
   cannot acknowledge an unrelated terminal.

## Consequences

### Positive

- Live failure and crash recovery now use the same side-effect certainty rule.
- A transport timeout or executor exception cannot make an unclassified remote
  Tool look completed merely because the Worker process stayed alive.
- Private executor diagnostics remain operator-local and do not enter durable
  events, Tool Results or model context.
- The independent Host can finish an indeterminate Run, accept operator
  evidence and continue without replaying the original side effect.

### Negative and incomplete

- A wrong Tool effect declaration is still dangerous; the immutable descriptor
  and signed Skill admission must remain authoritative.
- The NATS publication branch was not exercised against a live NATS server in
  this local-only stage.
- Real MCP transport ambiguity after a remote server accepts a call but drops
  the response still needs its own HTTP/stdio closed-loop gate.
- Real Linux cgroup, PTY and Windows process supervision remain incomplete.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/core/src/tools/{events,parallel,registry}.rs`
- OpenClaw revision `58b4b9430457`,
  `packages/agent-core/src/agent-loop.ts` and
  `src/agents/bash-tools.exec-output.ts`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `docs/evidence/2026-08-11-effect-aware-live-tool-failure.md`
