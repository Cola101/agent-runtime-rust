# ADR-0055: Checkpoint-first subagent message receipts

## Status

Accepted and implemented in the protocol-neutral Worker and standalone Rust
Host. The local crash and replay boundary is behaviourally verified. Running
child input/interrupt and full child conversation continuity remain outside
this decision.

## Context

ADR-0054 made a stable asynchronous handle possible, but `agent.send` only
stored a monotonic sequence. A caller retry while the child was active was
rejected, and a crash between exposing the Tool result and launching the
process-local task could leave an acknowledged input without an automatic
executor.

Codex `ff352fab6209` returns a submission ID and supports optional interrupt,
but the inspected handler relies on its resident Thread control path rather
than a caller-keyed, cross-process receipt. OpenClaw `58b4b9430457` has durable
outbox/replay/generation state for completion delivery, while `sessions_send`
itself returns a run ID and does not expose a strict caller idempotency key.

The standalone Runtime needs a transport-neutral rule that remains valid when
the future caller is a model Tool, SDK, local daemon or distributed control
plane.

## Decision

1. `agent.send` requires `idempotency_key`: 1–128 portable ASCII characters.
   The key is scoped to one stable `agent_id`.
2. Worker Checkpoint schema 14 stores a per-handle message receipt containing
   the key, exact message digest, monotonic sequence, deterministic submission
   ID and the complete successor child request.
3. Replaying the same key and message returns the original receipt whether the
   successor is active, terminal or the handle was later closed. It never
   increments the sequence or creates another child Run. Reusing the key with
   different content fails with `SubagentMessageConflict`.
4. A new send is valid only for a terminal, open handle. It continues to clamp
   role capabilities and Token/cost/duration budget to the parent remainder.
5. The Host mutates the Worker state and Tool transcript, atomically replaces
   the local Checkpoint, and only then emits the acceptance/Tool-result events
   and launches the process-local task. Therefore an externally observable
   acknowledgement always has a durable receipt.
6. On Host replacement, every active asynchronous request is launched eagerly
   from the restored parent/child Checkpoints. `agent.wait` observes progress;
   it is no longer required to activate acknowledged work.
7. The successor Run ID remains deterministic from parent execution identity,
   stable handle and message sequence. Recovery may retry an interrupted model
   transport, but it cannot accept a second logical message or allocate a
   second logical child Run.

## Consequences

### Positive

- Caller retry is safe during active execution and after process replacement.
- “Acknowledged but never launched” is closed without adding NATS, a database
  or another mandatory service.
- Conflicting key reuse is explicit rather than silently choosing one payload.

### Negative and incomplete

- Filesystem Checkpoint replacement is the authority only for the standalone
  Host. A distributed adapter still needs an equivalent transactional store.
- A hard crash during a provider stream may retry that provider request under
  the same logical child Run; provider-level exactly-once is neither possible
  nor claimed.
- Running-child message queues, `interrupt=true`, rich input items and full
  persisted child transcript/compaction remain the next lifecycle gap.

## References

- ADR-0054: persistent asynchronous subagent handles
- Codex `codex-rs/core/src/tools/handlers/multi_agents/send_input.rs`
- OpenClaw `src/agents/tools/sessions-send-tool.ts`
- OpenClaw `src/agents/subagent-registry.types.ts`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
