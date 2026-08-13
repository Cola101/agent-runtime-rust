# ADR-0054: persistent asynchronous subagent handles

## Status

Accepted for the explicit asynchronous mode in the protocol-neutral Worker and
standalone Rust Host. This is the first persistent interaction slice, not full
parity with Codex threads or the OpenClaw subagent registry. ADR-0055 extends
the schema 13 state to schema 14 with caller-keyed message receipts.

## Context

The standalone Host originally treated `agent.spawn` as one blocking Tool:
the parent waited until the child reached a terminal state and received the
result under the original Tool call. That preserves simple Tool ordering, but
cannot support a long-lived child, bounded waiting, later input, targeted
close, or recovery through a stable public identity.

Codex `ff352fab6209` returns a thread/agent ID immediately, keeps parent edges
durable, waits through status subscriptions, sends input to a resident thread
and can interrupt or close the live agent tree. OpenClaw `58b4b9430457` keeps a
more detailed operational registry covering execution, completion, delivery,
generation, timeout, pause and kill reconciliation.

The Runtime needs those interaction semantics without making Java, NATS,
PostgreSQL or a container runtime mandatory for one local Run.

## Decision

1. `agent.spawn` keeps the legacy `inline` default for existing contracts and
   adds an explicit `async` mode. Async spawn writes the request and stable
   `agent_id` to the parent Checkpoint before launching the child, then returns
   immediately.
2. The stable `agent_id` is the initial delegation ID. Every continuation is a
   new deterministic child Run ID under that handle; child Runs retain their
   own lineage, events, Checkpoint and clamped budget.
3. Worker Checkpoint schema 13 stores active requests, terminal deliveries,
   stable handle templates, monotonic per-handle message sequence and an
   irreversible closed-handle set. The process-local task map is only a cache.
4. `agent.wait` has a bounded timeout. Timing out does not cancel the child.
   A later wait, including after Host replacement, recreates missing process
   state from the parent and child Checkpoints and observes the same handle.
5. `agent.send` currently accepts input only after the previous child turn is
   terminal. Acceptance, sequence and successor request are Checkpointed before
   the successor task launches. Permissions and budgets can only shrink.
6. `agent.close` cancels the target child token, waits for its real terminal
   result, reaps the task, and persists `subagent.closed`. Repeated close is
   idempotent; a closed handle remains readable but can never accept input,
   including after recovery.
7. Parent cancellation, duration expiry, terminal completion and explicit Host
   shutdown cancel and await every live child task with bounded cleanup.
8. One model turn may not mix legacy inline spawn and async spawn. This avoids
   ambiguous Tool result ordering during migration.

## Consequences

### Positive

- The standalone Rust Runtime now has a recoverable handle rather than a
  process-local task ID.
- Wait timeout, targeted close and parent-tree cleanup have real resource
  semantics, not status-only responses.
- Terminal result and usage settlement remain replay-safe, and closed handles
  cannot be resurrected after a crash.

### Negative and incomplete

- `agent.send` starts a successor child Run with role instructions and new
  input, but does not yet restore the complete child transcript or compressed
  context. It is not a Codex persistent Thread.
- Sending to a running child, queued delivery and `interrupt=true` remain
  incomplete. Durable per-message receipts and recovery between send
  acceptance and child launch are implemented by ADR-0055.
- The standalone state is one filesystem authority. OpenClaw-style delivery
  retry, generation reconciliation and multi-process ownership are not part of
  this stage.

## References

- Codex `codex-rs/core/src/tools/handlers/multi_agents/{spawn,wait,send_input,close_agent}.rs`
- Codex `codex-rs/core/src/agent/control/{spawn,legacy}.rs`
- OpenClaw `src/agents/tools/sessions-{spawn,send,yield}-tool.ts`
- OpenClaw `src/agents/subagent-registry.types.ts`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
- `runtime/apps/worker/tests/assignment.rs`
