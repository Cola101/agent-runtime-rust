# ADR-0059: Protocol-neutral subagent transcript capsule

## Status

Accepted and implemented in the protocol crate, Worker and standalone Rust
Host. Exact child Tool history, terminal Checkpoint ordering and replacement-
Host recovery are behaviourally verified without an external control plane.

## Context

Stable subagent handles previously persisted each completed turn as only the
original User input and a flattened Assistant result. A child Run could already
contain Assistant narrative, Tool calls, bound Tool results and a final answer,
but that typed history was lost at the Host result boundary. A later
`agent.send` therefore lacked the evidence the child model had actually seen.

This also left a crash window: the child terminal event could be durable while
the parent result sidecar was not, and the last non-terminal child Checkpoint
could not reconstruct the completed transcript.

Codex `ff352fab6209` stores rich `ResponseItem` history and repairs call/output
pairs during normalization. OpenClaw `58b4b9430457` stores rich Session entries
and repairs missing or orphan Tool results when rebuilding model context.

## Decision

1. `SubagentResultDelivery` carries a provider-neutral `Vec<Message>` transcript.
   It excludes the child System message so a delegated instruction cannot be
   replayed as new authority in later turns.
2. Result digest v3 binds source, outcome, usage and the exact transcript. The
   encoded transcript is capped at 1 MiB.
3. A rich transcript must begin with User, end with Assistant on successful
   completion, and contain unique Tool Call IDs with exactly one later bound
   Tool Result. Missing, orphan and duplicate pairs fail closed.
4. RunExecution schema 14 requires every new non-empty child history turn to
   carry a rich transcript. Schemas 12 and 13 cannot carry it, preventing a
   downgrade from disguising new state as an older contract. Legacy text-only
   history remains readable under its original schema.
5. Worker Checkpoint schema 18 stores the typed nested transcript and rejects an
   older Checkpoint that claims to contain the new state.
6. The standalone Host writes a terminal child Checkpoint before publishing a
   terminal child event. The Checkpoint transcript can therefore be recovered
   before the parent result receipt becomes durable.
7. Normal completion obtains the transcript directly from the child Worker.
   Replacement-Host recovery obtains it from the verified terminal child
   Checkpoint and verifies Run identity and terminal status. Legacy empty
   transcripts continue through schema 13 rather than being relabelled v14.

## Consequences

### Positive

- A stable subagent handle preserves the exact model-visible Tool conversation
  across follow-up messages and Host replacement.
- Result integrity covers usage and typed history, not only flattened text.
- Recovery after child completion does not have to replay a completed Tool.
- No provider wire type or external service enters the standalone Kernel path.

### Negative and incomplete

- Authoritative state deliberately fails closed; there is not yet a separate,
  auditable repair mode for imported or truncated histories.
- The protocol does not yet represent provider-private reasoning items,
  encrypted content, multimodal attachments or all Codex/OpenClaw item types.
- Fork, rollback and Session-tree branch/reset lifecycle are not implemented.
- Terminal Checkpoint-before-event ordering is verified for the standalone
  filesystem Host. The optional cloud transport does not yet publish the same
  terminal Checkpoint sequence and must not inherit this claim.
- Event-log append, Checkpoint replacement and parent receipt remain separate
  filesystem writes; recovery reconciles them but does not make them atomic.

## References

- Codex `codex-rs/core/src/context_manager/{history,normalize}.rs`
- OpenClaw `packages/agent-core/src/harness/session/session.ts`
- OpenClaw `src/agents/session-transcript-repair.ts`
- `contracts/events/run-execution-requested.v14.example.json`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/crates/protocol/tests/subagent_recovery_contract.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
