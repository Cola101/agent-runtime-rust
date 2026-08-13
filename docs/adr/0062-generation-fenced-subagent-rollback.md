# ADR-0062: Generation-fenced persistent subagent rollback

## Status

Accepted and implemented in the protocol-neutral Worker, Kernel event stream
and standalone Rust Host. A real model/Tool/model loop proves immutable prior
generations, stale-generation fencing and absence of Tool replay.

## Context

Fork created an independent handle from a completed history prefix, but a
stable handle could not abandon a bad suffix without deleting history or
changing identity. Destructive vector truncation would erase audit facts;
copying every full generation would multiply a bounded 2 MiB history across
every rollback.

Codex `ff352fab6209` appends `ThreadRolledBack` markers to persisted rollout
history, reconstructs the effective view by replay and rejects rollback while a
turn is active. OpenClaw `58b4b9430457` rotates the Session identity under a
stable key, archives reset transcripts, clears queued work and uses lifecycle
ownership to reject pre-reset writes. These are different product operations,
but both retain an authoritative old timeline and fence concurrent work.

## Decision

1. `agent.rollback` requires a stable terminal handle, the caller-observed
   generation and an older completed `activation_ordinal` in the current head.
   Active, queued, closed, stale-generation and no-op boundaries are rejected.
2. The handle ID, role, delegated scopes and budget cap do not change. A
   successful transition increments generation by exactly one and emits
   `subagent.rolled_back` with both history digests.
3. Activation ordinals are global monotonic audit sequence numbers for the
   handle. Rollback does not reuse removed ordinals; the next turn after
   `[0, 1] -> [0]` receives ordinal `2`.
4. Superseded Turn payloads are stored once by activation ordinal. Each old
   generation stores only its ordered ordinal head and digest. Current and
   archived histories are therefore materialized without cloning every full
   generation.
5. Checkpoint schema 21 persists archived Turns, generation heads and
   idempotent rollback records. It validates every historical digest, rejects
   unreferenced/tampered archive data and bounds one handle to 32 generations,
   512 archived Turns and 8 MiB archived JSON.
6. `agent.history` can read an explicit immutable generation. Omission selects
   the current head. `agent.send` exposes generation in its model schema and
   requires an exact value after the handle has advanced beyond generation 1.
7. Continuation bindings include generation. A late old-generation result or
   stale command cannot settle a newer active head.
8. The Host checkpoints the generation transition before exposing its Tool
   result. Recovery in that crash window returns the same receipt and event,
   not another generation increment.

## Consequences

### Positive

- Rollback changes an effective head without deleting conversation or audit
  history.
- Stable handles remain useful to callers while generation supplies a precise
  concurrency fence.
- Historical Assistant Tool Call/Result pairs remain model context only and do
  not re-enter pending execution.
- The implementation runs without Java, PostgreSQL, NATS, Docker or
  Kubernetes.

### Negative and incomplete

- Rollback is handle-scoped, not yet a general root Thread/Session operation.
- Only an inclusive completed activation boundary is supported; there is no
  `num_turns`, empty-history target, redo or forward generation switch.
- Archived-history limits intentionally reject unbounded local retention;
  production retention/export policy remains outside the standalone Kernel.
- Rich provider-private reasoning and multimodal items are not in the history
  IR.

## References

- Codex `codex-rs/core/src/session/handlers.rs`
- Codex `codex-rs/core/src/{thread_rollout_truncation.rs,session/rollout_reconstruction.rs}`
- OpenClaw `src/gateway/{session-reset-service.ts,session-lifecycle-state.ts}`
- OpenClaw `src/auto-reply/reply/session-reset-cleanup.ts`
- `runtime/crates/{protocol,kernel}/src/lib.rs`
- `runtime/apps/{worker,runtime-host}/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
