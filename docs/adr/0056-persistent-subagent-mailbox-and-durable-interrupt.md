# ADR-0056: Persistent subagent mailbox and durable interrupt

## Status

Accepted and implemented in the protocol-neutral Worker and standalone Rust
Host. FIFO delivery, interrupt, schema migration, malformed-state rejection and
Host replacement are behaviourally verified without an external service.

## Context

ADR-0055 made one terminal-handle follow-up durable, but a running child still
rejected new input. That left two gaps: ordinary input could not wait behind an
active turn, and an urgent redirect could not stop that turn without losing its
message at a process boundary.

Codex `ff352fab6209` implements `send_input(interrupt=true)` by submitting an
Interrupt operation before UserInput to a resident Thread. OpenClaw
`58b4b9430457` exposes separate steering/follow-up queues and run abort controls,
with more mature generation and delivery lifecycle. Neither process-local
ordering alone proves that an accepted redirect survives a Host crash.

## Decision

1. Worker Checkpoint schema 15 stores a per-handle `VecDeque` of caller keys.
   Every accepted message receives a fixed successor request and a receipt in
   `queued`, `active`, `completed` or `cancelled` state.
2. A non-interrupting send to a running child appends to the FIFO queue. The
   next message activates only after the current child has a bound terminal
   result. The Host reconciles finished tasks even when the parent has not
   called `agent.wait`.
3. Admission reserves the active request plus every queued request against the
   remaining parent Token, cost and duration budgets. A handle accepts at most
   eight queued messages.
4. `agent.send` accepts `interrupt=true`. The receipt and Tool result are
   checkpointed before the active child cancellation token is triggered. An
   interrupt is placed at the front of the mailbox, because it is an immediate
   redirect rather than an ordinary follow-up.
5. The Host waits for the old child to produce a real bound terminal result,
   records its actual usage once, activates the redirect, checkpoints the new
   authority and only then launches the replacement child.
6. A replacement Host scans durable pending interrupts before relaunching
   ordinary active requests. Therefore a crash after acceptance cannot resume
   work that the caller redirected.
7. Key replay also binds the interrupt flag. The same key with changed text or
   changed interrupt intent is a conflict; exact replay returns the original
   receipt and cannot cancel or launch again.
8. Restore migrates schema 14 receipts to their derived active/completed state.
   Schema 15 restore rejects missing, duplicate, wrongly keyed or status-mismatched
   mailbox receipts instead of silently restarting an old child.
9. Closing a handle cancels queued receipts and removes its mailbox. A closed
   handle cannot be revived by either a normal or interrupting send.

## Consequences

### Positive

- Running input, urgent redirect and Host replacement share one durable state
  machine rather than three process-local code paths.
- Interrupt is stronger than a best-effort signal: its replacement message is
  recoverable before the old child is stopped.
- FIFO order, queue bounds and budget reservation remain inspectable in the
  Checkpoint and independent of Java, NATS or a database.

### Negative and incomplete

- Each message still creates a new logical child Run. The stable handle does
  not yet restore a complete Thread transcript or compacted equivalent context.
- Interrupt deliberately overtakes ordinary queued messages. This priority rule
  is part of the public semantics and must not be described as strict FIFO
  across urgent and non-urgent messages.
- Provider requests cannot be made exactly once across a hard transport crash;
  only Runtime acceptance, child identity and budget settlement are deduplicated.
- Rich input items, attachments, history querying and generation-level operator
  controls remain outside this decision.

## References

- ADR-0054: persistent asynchronous subagent handles
- ADR-0055: Checkpoint-first subagent message receipts
- Codex `codex-rs/core/src/tools/handlers/multi_agents/send_input.rs`
- Codex `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
- OpenClaw `packages/agent-core/src/agent.ts`
- OpenClaw `packages/agent-core/src/agent-loop.ts`
- OpenClaw `src/talk/agent-run-control.ts`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
