# ADR-0052: bounded parallel subagent supervision and budget settlement

## Status

Accepted and implemented for adjacent `agent.spawn` calls in the standalone
Rust Host. The batch is limited to eight children, survives process restart,
shares cancellation, and settles digest-bound child model usage into the
parent. Long-lived child messaging remains follow-up work.

## Context

ADR-0049 to ADR-0051 proved serial role delegation, child recovery, downward
cancellation and nested Tool approval. Serial execution was incorrect for one
model turn containing independent spawn calls: the first child could block the
second indefinitely, even though both intents belonged to the same completed
model turn. It also checked each requested child budget independently, allowing
their sum to exceed the parent balance, and released the reservation without
charging actual child usage.

Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c` creates a long-lived child
agent, returns its ID immediately, and uses agent-registry spawn/residency slots
plus explicit wait, send and close operations. OpenClaw
`58b4b9430457e91b44f0ccce73ad1b6c6bb11e28` has a persistent subagent registry,
FIFO group reservations, per-parent/group concurrency caps, a collector that
waits for children to settle, and orphan recovery. This Host intentionally
keeps the current blocking Tool contract, but must not serialize independent
children or lose durable accounting.

## Decision

1. Consecutive `agent.spawn` calls from one model Tool turn form one ordered
   batch. An ordinary Tool is an ordering barrier and is not run concurrently
   with that batch.
2. The Worker admits at most eight unresolved children for one parent. It
   rejects the ninth before consuming its Tool call or mutating Kernel state.
3. Admission subtracts both actual parent usage and all unresolved child Token
   and cost reservations. Duration is wall-clock rather than additive; the
   shared active-time enforcement is specified by ADR-0053.
4. The complete ordered request batch is written in Checkpoint schema 11 before
   any child starts. A legacy single `pending_subagent` projection remains for
   old readers; restore upgrades old single-request snapshots in memory.
5. The Host runs the batch through unordered futures but records results in
   original Tool Call order. Each child persists an atomic, delegation-specific
   result receipt before the parent consumes it.
6. A restarted Host reuses completed receipts and resumes only unfinished child
   Checkpoints. Replayed identical results return the original parent receipt;
   a conflicting digest fails closed.
7. One root cancellation token fans out to every child. Parent cancellation
   closes every in-flight model stream and emits one parent terminal event.
8. A child terminal failure is a bound error result, including terminal status
   and `is_error`; successful siblings remain available to the parent model.
   Infrastructure failure of the supervisor itself still fails the parent.
9. A child result carries actual Token and micro-cost usage. Non-zero usage is
   included in a versioned result digest and is accumulated into the parent
   Checkpoint exactly once. Legacy zero-usage result digests remain verifiable.
10. When children park on approvals, completed siblings are persisted first and
    the first approval in Tool Call order is projected to the parent. Resolution
    routing walks durable child lineage and rejects another subtree.

## Consequences

### Positive

- Independent children are truly simultaneous without adding a database,
  queue, control plane, Docker or long-lived Agent service.
- Crash recovery does not replay completed children, and result completion
  order cannot corrupt Tool Call binding.
- Parent budget checks cover concurrent reservations and actual child usage;
  duplicated deliveries cannot charge twice.
- Cancellation, failure and approval use the same durable Run/Checkpoint
  semantics as serial execution.

### Negative

- The fixed limit is per parent, not a configurable tenant/runtime scheduler.
- `agent.spawn` remains a blocking Tool batch. Unlike Codex and OpenClaw, there
  is no child ID returned for later wait/send/close interaction.
- Standalone pricing is deliberately zero, so non-zero cost settlement is
  contract-tested rather than vendor-billed in local mode.
- Multiple simultaneous approvals are surfaced one at a time; there is no
  authenticated multi-reviewer CAS or audience projection.

## Failure modes

- Ninth child or cumulative budget overflow: reject before state mutation.
- Daemon crash after one child completes: reuse its atomic receipt, resume only
  children without receipts.
- Parent cancellation during streams: close every child request and terminate
  the parent once.
- Child terminal failure: return a bound error result; do not erase siblings.
- Result replay: return the original receipt without adding usage again.
- Result or usage tampering: digest mismatch, fail closed.
- Decision targets another subtree: reject before executing its Tool.

## References

- ADR-0049: standalone role subagent execution
- ADR-0050: standalone child cancellation and recovery
- ADR-0051: standalone nested approval routing and recovery
- ADR-0053: recoverable tree execution duration budget
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/crates/protocol/tests/subagent_recovery_contract.rs`
