# ADR-0061: Generation-bound persistent subagent fork

## Status

Accepted and implemented in the protocol-neutral Worker, Kernel event stream
and standalone Rust Host. A real model/Tool/model loop proves branch isolation,
typed Tool-history inheritance and absence of Tool replay.

## Context

Durable subagent handles could continue one append-only conversation but could
not explore an earlier completed point without mutating that handle or importing
its history through a lower-authority repair path. Copying an active request,
mailbox or process would also duplicate execution intent and reserved budget.

Codex `ff352fab6209` exposes `thread/fork`, allocates a new thread identity and
supports latest, through-turn and before-turn boundaries. Its thread store keeps
fork lineage and rejects in-progress inclusive boundaries. OpenClaw
`58b4b9430457` can fork a requester transcript into a new Session, restricts
visible forks to the same target agent and can fall back to isolated context
when the transcript cannot be safely prepared.

## Decision

1. `agent.fork` requires a source stable handle, the caller-observed source
   generation, one completed `activation_ordinal`, and a finite per-turn budget
   cap. A stale generation, missing boundary or budget increase is rejected.
2. The fork identity is deterministic over tenant, parent Run, Tool call,
   source handle and boundary. Replaying the same in-flight Tool after recovery
   returns the same handle and the same `subagent.forked` event.
3. The new handle starts at generation 1 and records source handle, source
   generation, boundary and exact source-prefix digest. `agent.history` returns
   the current generation and fork provenance.
4. Only the completed protocol-neutral conversation prefix is cloned. Active
   child requests, queued messages, message receipts, close state and process
   tasks are not copied.
5. The role is fixed to the source role, so delegated scopes cannot expand.
   The fork budget must not exceed the source handle cap or the parent Run's
   remaining token, cost and active-time budget.
6. The selected terminal result is a branch-head reference; the first
   `agent.send` creates a new child Run and advances only the fork. Source and
   fork histories thereafter append independently.
7. Worker Checkpoint schema 20 persists generation indexes and idempotent Fork
   records. Schema 19 and older handles migrate to generation 1; an older schema
   carrying branch state is rejected.
8. Completed Assistant Tool Call/Result items remain model-visible history but
   never enter a pending Tool queue. Fork does not use the external-history
   repair path and does not raise conversation data to System authority.

## Consequences

### Positive

- Branching is an auditable immutable-history operation rather than mutation of
  the source handle or replay of live work.
- Generation binding gives the following Rollback phase a stale-head fence.
- The implementation is provider-neutral and needs no Java, NATS, PostgreSQL,
  Docker or Kubernetes.

### Negative and incomplete

- Fork is currently handle-scoped, not a general root Session/Run API.
- Only an inclusive completed activation boundary is exposed; Codex also has
  latest and before-turn modes plus paginated lineage materialization.
- The role is preserved exactly. Explicit scope reduction to a smaller subset
  is not yet exposed, although expansion is impossible.
- Rich provider-private reasoning, encrypted items and multimodal attachments
  remain outside the transcript IR.
- Handle-scoped Rollback is now implemented by ADR-0062; general root
  Thread/Session branching remains outside this ADR.

## References

- Codex `codex-rs/thread-store/src/{types.rs,local/paginated_fork.rs}`
- Codex `codex-rs/app-server/src/request_processors/thread_processor.rs`
- OpenClaw `src/agents/subagent-spawn-context.ts`
- OpenClaw `src/auto-reply/reply/session-fork.ts`
- `runtime/crates/{protocol,kernel}/src/lib.rs`
- `runtime/apps/{worker,runtime-host}/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
