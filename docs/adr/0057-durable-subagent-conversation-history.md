# ADR-0057: Durable subagent conversation history

## Status

Accepted and implemented in the protocol-neutral Worker and standalone Rust
Host. Role-preserving continuation, history pagination, schema migration,
malformed-state rejection and Host replacement are behaviourally verified
without an external service.

## Context

ADR-0056 made a stable subagent handle durable, but every successor child Run
started with only its new input. The handle therefore preserved lifecycle and
delivery identity while silently losing the conversation that identity implied.
Flattening earlier turns into agent instructions would be worse: untrusted
conversation content would gain system-message authority.

Codex `ff352fab6209` sends follow-up input to a resident Thread and can reload a
V2 agent with its history. It also has full-history/last-N fork modes and
compaction. OpenClaw `58b4b9430457` owns `mutableState.messages`, appends model
events to it, drains steering/follow-up queues into the same transcript, and
rebuilds Session context across compaction/reset boundaries.

## Decision

1. RunExecution schema 12 carries `subagent_history`, an ordered,
   provider-neutral list of completed handle turns. A turn binds activation
   ordinal, caller message sequence, child Run identity, input and the verified
   terminal result.
2. The Worker adapter maps every inherited turn to a user message followed by
   an assistant message, then appends the current user input. Conversation data
   is never concatenated into system instructions.
3. Worker Checkpoint schema 16 stores the completed conversation and last
   activation ordinal for every stable handle. Acceptance order remains in
   `message_sequence`; actual execution order is in `activation_ordinal`, because
   an interrupt may overtake an older FIFO message.
4. An immediate continuation freezes the current history before admission. A
   queued continuation is a two-phase intent: its deterministic child identity
   and budget are accepted first, then its exact history and binding digest are
   finalized when it is activated. The activation event is the authority for
   the final execution binding.
5. A result is appended only after child identity, request binding and result
   digest validate. The next child request binds the SHA-256 digest of the exact
   history prefix it receives.
6. History is capped at 128 completed turns and 2 MiB. Exceeding the cap fails
   closed; this is a safety bound, not a substitute for compaction.
7. `agent.history` is a pure, read-only Tool. It returns at most 50 completed
   turns after an optional activation cursor, plus current status, queue depth
   and close state. It uses the parent's existing `agent:spawn` authority and
   grants no new Workspace, Tool or model permission.
8. Schema 16 restore rejects missing handle indexes, malformed turns, divergent
   activation ordinals, active children bound to another history prefix and
   completed results that do not match the conversation tail.
9. Schema 15 and older checkpoints cannot reconstruct data they never stored.
   Migration preserves a verifiable latest completed turn when possible and
   otherwise resumes with an empty legacy prefix; it never invents dialogue.

## Consequences

### Positive

- A stable handle now means a stable model-visible dialogue, including after a
  Host crash, rather than merely a stable lifecycle identifier.
- Interrupt priority is auditable without corrupting chronological context.
- The representation is independent of OpenAI, Anthropic or local provider
  message formats and runs without Java, NATS, PostgreSQL or Docker.
- History remains lower-authority conversation input and is queryable through a
  bounded, side-effect-free interface.

### Negative and incomplete

- The capsule preserves handle-level user input and terminal assistant result,
  not the full internal child Tool Call/Result transcript, reasoning items or
  multimodal attachments.
- There is no compaction summary or retained-tail policy. Once the hard bound is
  reached, new input is rejected instead of degrading context silently.
- Each message is still a distinct child Run. Codex remains ahead on resident
  Thread semantics, fork/rollback and complete rollout reconstruction.
- OpenClaw remains ahead on long-lived Session repair, compaction boundaries,
  generation governance and rich transcript item types.
- Provider calls remain at-least-once across a hard transport crash; Runtime
  acceptance, history identity and budget settlement are deduplicated.

## References

- ADR-0054: persistent asynchronous subagent handles
- ADR-0055: Checkpoint-first subagent message receipts
- ADR-0056: persistent subagent mailbox and durable interrupt
- Codex `codex-rs/core/src/tools/handlers/multi_agents/send_input.rs`
- Codex `codex-rs/core/src/agent/control/spawn.rs`
- Codex `codex-rs/core/src/compact.rs`
- OpenClaw `packages/agent-core/src/agent.ts`
- OpenClaw `packages/agent-core/src/harness/session/session.ts`
- OpenClaw `packages/agent-core/src/harness/compaction/compaction.ts`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
