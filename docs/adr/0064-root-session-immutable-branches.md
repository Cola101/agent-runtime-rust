# ADR-0064: Immutable root Session branches and generation-fenced Rollback

## Status

Accepted and implemented in the protocol-neutral Worker and standalone Rust
Host. Real HTTP/SSE model and MCP Tool loops cover Fork, Rollback and two
different crash-recovery boundaries.

## Context

The Runtime already had generation-bound Fork/Rollback for persistent subagent
handles, but every standalone root Run still set `session_id = run_id`. It had
no stable root Session identity, no independent branch, and no authoritative
completed-turn prefix. Reusing `history_import` would have been incorrect: that
API deliberately treats external/truncated messages as lower-authority damaged
input, while root Session history is Runtime-owned state.

Codex `ff352fab6209` keeps a stable Thread, appends `ThreadRolledBack` markers,
reconstructs the effective rollout and token usage, and rejects Rollback while
a Turn is active. OpenClaw `58b4b9430457` rotates the concrete Session under a
stable key, archives the old transcript, aborts active work, clears queues and
uses lifecycle generations/owners to reject work from the retired Session.

## Decision

1. RunExecution schema 16 adds `session_branch`, separate from explicit
   `history_import` and child `subagent_history`. A root v16 command must carry
   exactly one well-formed branch; a delegated child must not carry one.
2. `session_id` is the stable logical Session identity. `branch_id` identifies
   one independently advancing branch, `generation` fences Rollback, and each
   Run remains a unique execution/attempt identity.
3. A completed `SessionConversationTurn` contains only that Turn's
   provider-neutral user/assistant/Tool transcript. System authority and
   inherited history are excluded; Tool Call/Result pairs, terminal Assistant,
   ordinal, Run identity and SHA-256 digest are mandatory.
4. The local Host persists `sessions/{session_id}/session.json` before starting
   a Turn. The active binding contains Run, generation, history digest and
   input. Continue, Fork and Rollback reject stale generations and reject any
   branch with an active Turn.
5. Fork creates a new branch at generation 1 from an inclusive completed
   prefix. It copies no active process state. Rollback archives the complete
   previous generation, advances generation by exactly one and moves the
   effective head to an earlier completed prefix without deleting history.
6. Worker admission flattens authoritative completed Turns into ordinary model
   context. Historical Tool Calls never enter `pending_tool_calls`. Worker
   Checkpoint schema 23 persists the exact branch snapshot and recovery rejects
   any generation or history drift.
7. A Session-bound terminal transcript Checkpoint is written before the
   terminal event becomes observable. If the Host dies before advancing the
   Session file, a replacement validates Session/branch/generation/input,
   terminal event and Checkpoint together, then commits the Turn without a new
   model or Tool invocation.
8. The local state root remains single-writer through the existing daemon lock.
   Distributed branch transactions and tenant retention are not inferred from
   this filesystem implementation.

## Consequences

### Positive

- Root identity is no longer conflated with one Run.
- Fork and Rollback preserve typed Tool history without replaying side effects.
- Active work, stale callers, stale Checkpoints and late terminal commits share
  one generation/history fence.
- Provider failure and the terminal-event/head-commit crash window recover
  without Java, PostgreSQL, NATS, Docker or Kubernetes.

### Negative and incomplete

- Archived root generations currently retain complete history snapshots; this
  is simpler and auditable but duplicates storage compared with Codex markers
  or the subagent handle's deduplicated ordinal graph.
- The public Rust Host API exposes Session operations, but local IPC/CLI and a
  future GUI do not yet expose them.
- Rollback refuses active Turns instead of aborting them. There is no OpenClaw-
  style reset cascade, queue cleanup hook system, redo, branch deletion,
  retention/export policy or distributed compare-and-swap store.
- The provider-neutral transcript still lacks Codex/OpenClaw reasoning-private
  items and several rich multimodal/provider-specific message variants.

## References

- Codex `codex-rs/core/src/session/handlers.rs`
- Codex `codex-rs/core/src/thread_rollout_truncation.rs`
- Codex `codex-rs/core/src/session/rollout_reconstruction.rs`
- OpenClaw `src/gateway/session-reset-service.ts`
- OpenClaw `src/gateway/session-lifecycle-state.ts`
- OpenClaw `src/auto-reply/reply/session-reset-cleanup.ts`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
