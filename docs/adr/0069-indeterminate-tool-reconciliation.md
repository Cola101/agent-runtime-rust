# ADR-0069: Indeterminate Tool outcome and explicit reconciliation

## Status

Accepted and implemented in the standalone Rust Runtime. A real child process,
durable Checkpoint interruption and replacement Host prove that an ambiguous
side effect is not replayed. Versioned operator reconciliation starts a new Run
without mutating the source terminal Run.

## Context

A non-idempotent or unknown Tool may complete its external side effect after
`tool.execution.started` is durable but before a bound Tool result reaches the
Checkpoint. A replacement Host cannot safely infer whether the effect happened.
Retrying may duplicate a payment, message or write; silently failing loses the
evidence required for an operator to resolve the incident.

Codex has mature local Tool/session lifecycle and history normalization, but the
focused source path did not expose a generic durable operator reconciliation
contract for an ambiguous Tool side effect. OpenClaw dead-letters deliveries
when restart evidence shows ambiguous or committed side effects. That avoids an
unsafe retry, but it is not a protocol-neutral Agent Tool continuation contract.

## Decision

1. Restore accepts a Checkpoint containing exactly one started, unfinished
   `NonIdempotent` or `Unknown` Tool so it can classify the outcome. Multiple
   ambiguous calls fail closed. Pure and idempotent calls retain their existing
   safe retry path.
2. Recovery produces `TerminateIndeterminate`, then the Kernel emits one stable
   `run.indeterminate` terminal event. The event binds Tool call/name, effect,
   sandbox, binding digest, source attempt, started event/sequence and
   `replay_safe=false`. The outstanding request and started event remain in the
   terminal Checkpoint as evidence.
3. The source Run is immutable after this terminal event. Reconciliation never
   changes it to succeeded or resumes its old Agent Loop.
4. Protocol schema 1 defines `Applied`, `NotApplied` and `Unresolved` decisions.
   Every command binds tenant, source Run, source terminal event, Tool call,
   Tool binding, operator, reconciliation identity and monotonically increasing
   version. Operator-supplied Tool Result content is bounded to 256 KiB.
5. `Unresolved` only records evidence. `Applied` and `NotApplied` require a
   bounded continuation input, append a model-visible Tool Result explaining the
   operator decision, and start a fresh Run. The reconciliation ID is the new
   Run ID, making crash recovery and exact duplicate submission deterministic.
6. An exact duplicate of the latest version is idempotent. A changed command at
   the same version, a stale version, a skipped version or an attempt to
   supersede a final decision fails closed. Only `Unresolved` may advance to the
   next consecutive version.
7. The local Host persists the reconciliation receipt atomically before any
   continuation model request. If it crashes after the receipt, a replacement
   Host reconstructs or resumes the deterministic continuation Run.
8. A new final decision refuses a reconciliation identity whose Run directory
   already exists. An exact duplicate may resume only after its own receipt was
   durably recorded, preventing an unrelated terminal Run from being returned
   as the continuation.

## Consequences

### Positive

- Restart cannot silently choose whether a side effect happened.
- Operators receive exact evidence and can defer a decision without triggering
  model or Tool work.
- A resolved incident continues with explicit lower-authority evidence in a new
  Run, preserving the source Run as an audit fact.
- The contract is independent of OpenAI, Anthropic, Java, NATS and any GUI.

### Negative and incomplete

- The standalone Host path is verified; the optional NATS adapter shares the
  terminalization code but has not been exercised against an external broker in
  this milestone.
- Operator authentication, authorization, tenant audit UI and database RLS
  belong to a later control-plane boundary and are not implied by this local
  command contract.
- This resolves one ambiguous Tool at a time. Parallel side-effecting Tools are
  still forbidden, so a multi-effect reconciliation protocol is unnecessary.
- Live-vendor behavior and remote MCP side-effect receipts remain unverified.

## References

- Codex `codex-rs/core/src/context_manager/history.rs`
- Codex `codex-rs/core/src/context_manager/normalize.rs`
- OpenClaw `packages/agent-core/src/agent-loop.ts`
- OpenClaw `src/gateway/server-restart-sentinel-agent-delivery.ts`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/crates/kernel/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
