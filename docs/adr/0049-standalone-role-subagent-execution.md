# ADR-0049: standalone role subagent execution

## Status

Accepted. The serial parent-child-result vertical slice is implemented. ADR-0050
adds active child-model cancellation, in-flight recovery and durable result
handoff; bounded parallel children and nested approval remain incomplete.

## Context

The Worker and protocol already carried role, lineage, delegated-scope, budget,
checkpoint and result-delivery contracts, but the standalone Rust Host always
published an empty role catalog and rejected every `agent.spawn` plan. Therefore
the advertised subagent semantics worked only when the Java control plane and
message bus created the child Run.

Codex commit `ff352fab6209dc0f9d13fc0036ed3f9404682b2c` gives each child an
independent thread, role-derived configuration and parent edge, with depth and
capacity admission plus wait/close/status operations. OpenClaw commit
`58b4b9430457e91b44f0ccce73ad1b6c6bb11e28` persists a capability envelope and
child registry, enforces parent ownership and sandbox inheritance, and reconciles
orphaned children after restart.

The local Host needs the same safety invariants without Docker, Java, PostgreSQL,
NATS or a hidden single-user Gateway.

## Decision

1. The shipped binary accepts an optional, bounded JSON role file through
   `AGENT_RUNTIME_LOCAL_SUBAGENT_CONFIG`. Roles enter the same immutable
   `RunExecutionCommand` contract as cloud execution; malformed names, duplicate
   roles, depth overflow and scope escalation remain protocol validation errors.
2. `agent.spawn` is visible only when the current Run has `agent:spawn` and a
   non-empty authorized role catalog. The requested Token, cost and duration
   budget must fit the parent's remaining budget before a child is planned.
3. The child uses a separate `WorkerProcessor`, Run ID, event log and Checkpoint.
   Its Run ID is the deterministic delegation ID, so one Tool Call cannot invent
   another child identity after recovery.
4. Child instructions are exactly the selected role instructions. Child scopes
   are exactly that role's subset; MCP servers and further roles are intersected
   with those scopes. The child receives the requested budget, never the parent's
   full budget. Depth 3 remains a protocol hard stop.
5. The parent Checkpoint containing the pending spawn is persisted before child
   execution. A successful child must have a durable terminal event identity.
   Its digest-bound result carries that event, child Run, delegation and original
   Tool Call identities back into the parent transcript before the next model turn.
   Idempotency receipts are keyed by Tool Call so one attempt may run multiple
   children sequentially without a prior child receipt blocking the next.
6. The first implementation is serial. It does not claim the target limit of
   eight active children or nested approval routing. Cancellation and recovery
   ordering are specified separately by ADR-0050.

## Consequences

### Positive

- Standalone execution now proves real role delegation and result re-entry using
  the same Kernel/Worker contracts as cloud mode.
- Child identity, permissions, budget and durable state are independently
  inspectable and cannot be expanded by the model.
- The implementation remains protocol-neutral and adds no external service.

### Negative

- A child that waits for human approval cannot yet surface that approval through
  the parent daemon lifecycle.
- Serial recursion cannot implement Codex-style wait/send/close or OpenClaw-style
  kill reconciliation and orphan recovery.

## Alternatives rejected

- **Create children through the Java control plane:** rejected because the Rust
  Runtime must execute independently.
- **Run the child inside the parent's Worker state:** rejected because attempt,
  budget, Checkpoint and cancellation ownership would be coupled and unauditable.
- **Spawn an untracked async task:** rejected because a child without deterministic
  lineage and durable state cannot be recovered or safely cancelled.
