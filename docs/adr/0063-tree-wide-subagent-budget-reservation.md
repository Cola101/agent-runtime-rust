# ADR-0063: Tree-wide subagent budget reservation ledger

## Status

Accepted and implemented in the protocol-neutral Worker and standalone Rust
Host. A real HTTP parent/child Agent loop proves that different persistent
handles cannot each reserve the same parent balance.

## Context

The Worker previously bounded adjacent `agent.spawn` calls, but
`agent.send` counted only the active and queued work of the target handle.
Two terminal handles could therefore each reserve the whole remaining Token,
cost and duration budget. Root model admission also ignored child Token
reservations. Checkpoint recovery reconstructed execution state but had no
independent invariant proving that every unsettled child owned exactly one
reservation.

Codex `ff352fab6209` has a shared in-process `RolloutBudget` for actual weighted
Token usage across a root Thread and its subagents, with per-Thread reminders
and a terminal exhaustion error. It does not reserve future child caps and does
not cover cost or duration. OpenClaw `58b4b9430457` has global/per-subagent
concurrency limits and normalized usage/cost telemetry, but no equivalent
parent-tree reservation ledger was found in the inspected subagent paths.

## Decision

1. Every pending, active or queued child execution owns one reservation keyed
   by its deterministic child Run ID. The entry binds stable handle ID, Tool
   call, child binding digest and finite Token/cost/duration caps.
2. `agent.spawn`, `agent.send` and `agent.fork` calculate availability from the
   same Run-wide ledger. A later handle receives only the unreserved balance;
   no per-handle calculation may recreate the parent remainder.
3. Parent model admission subtracts child reservations from Token capacity and
   refuses a new model call when unreserved Token capacity is zero. The
   provider-neutral request carries that remaining Token ceiling. Cost stays
   under the existing post-usage terminal; duration stays under the shared
   active-time watchdog rather than treating concurrent wall time as additive.
4. Child activation preserves the reservation. Rebinding a queued child to its
   now-current conversation digest updates the same entry rather than creating
   or releasing capacity.
5. A bound terminal result removes its reservation and then settles actual
   usage. Duplicate results return the prior receipt without charging or
   releasing twice. Closing a terminal handle releases cancelled queued work;
   parent terminal, cancellation, timeout and budget failure release all
   remaining entries.
6. Checkpoint schema 22 persists the ledger. Recovery derives the exact expected
   set independently from pending requests, active children and queued message
   receipts. Missing, extra, duplicate or altered reservations fail closed even
   when the outer Checkpoint digest is recomputed.
7. Schema 21 and earlier Checkpoints may migrate only by rebuilding that exact
   set. A legacy payload that claims schema 21 while carrying schema-22 ledger
   fields is rejected.

## Consequences

### Positive

- All stable handles compete in one deterministic parent budget domain.
- Crash recovery neither sells the same balance twice nor strands capacity.
- Reservation release is tied to result/close/terminal lifecycle edges rather
  than inferred from whichever handle a caller happens to inspect.
- The mechanism runs locally without Java, PostgreSQL, NATS, Docker or
  Kubernetes.

### Negative and incomplete

- Reservations are conservative maxima; unused child capacity is unavailable
  until result settlement or cancellation.
- Provider cost cannot be hard-capped mid-request by the current Model IR.
  Admission fences zero remaining cost, while actual provider overage is still
  handled by the existing post-usage budget terminal.
- The ledger is scoped to one parent Run. Cross-Run tenant quotas, weighted
  fairness and distributed admission belong to the later platform scheduler.
- Codex-style weighted Token policies and reminder delivery are not included.

## References

- Codex `codex-rs/core/src/{rollout_budget.rs,session/rollout_budget.rs}`
- Codex `codex-rs/core/src/session/mod.rs`
- OpenClaw `src/config/{agent-limits.ts,types.agent-defaults.ts}`
- OpenClaw `src/agents/usage.ts`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
