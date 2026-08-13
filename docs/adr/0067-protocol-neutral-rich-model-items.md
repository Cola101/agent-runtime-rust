# ADR-0067: Protocol-neutral rich model items and private-state provenance

## Status

Accepted and implemented in the standalone Rust Runtime. Deterministic real
HTTP/SSE and gRPC tests cover OpenAI Responses reasoning/refusal, Anthropic
thinking/signature, same-route replay, cross-route omission, Checkpoint
replacement, compaction and Session Continue/Fork/Rollback.

## Context

The original model IR retained text, Tool calls, Tool results and usage but
silently discarded reasoning items, refusal items and Anthropic thinking
blocks. That lost model continuity across Tool turns and recovery. Treating
those fields as ordinary assistant text would be worse: private chain state
could reach users, logs or a different Provider during failover.

Codex retains Responses reasoning summaries and encrypted continuation state in
its typed rollout history. OpenClaw retains thinking, signatures and redacted
thinking with Provider-specific replay rules. A multi-Provider Runtime needs
the same continuity while making provenance and downgrade explicit.

## Decision

1. `ContentPart` and `ModelStreamEvent` gain typed `Reasoning` and `Refusal`
   variants. Readable summaries are separate from `ProviderPrivateState`.
2. Private state is bounded and bound to Provider route id, protocol, model and
   format. It is replayed only when all four fields match the selected target.
3. A mismatch removes only the opaque state and emits
   `model.private_state.omitted` with origin, target and format. The event never
   contains opaque data and is audit-only, so it does not falsely commit model
   output or disable an otherwise safe zero-output fallback.
4. OpenAI Responses requests include `reasoning.encrypted_content`; returned
   reasoning id, summary and encrypted content become one typed item. Refusal
   completion becomes a typed refusal rather than a text delta.
5. Anthropic thinking text/signature and redacted thinking data remain opaque
   continuation state. They are never emitted as visible text and are replayed
   only to the same route/protocol/model.
6. Protobuf carries the same content and event variants. Worker transcript,
   Checkpoint, terminal transcript, compaction tail and Session branches retain
   them without flattening. Kernel public events expose summary/refusal or
   omission metadata, never private data.
7. A typed refusal is still a completed model response. The standalone Host
   returns its refusal text as the Run output so callers do not receive a blank
   successful result.

## Consequences

### Positive

- Tool turns, Host replacement and Session branches keep same-Provider model
  continuity without coupling the Kernel to OpenAI or Anthropic wire types.
- Cross-Provider failover degrades safely and visibly instead of leaking opaque
  state or silently dropping it.
- Audit-only events no longer count as committed Provider output.

### Negative and incomplete

- Opaque state is protected from events and cross-Provider egress, but the
  local Checkpoint file has no field-level encryption beyond the state-root
  security boundary.
- Anthropic thinking enablement budgets, prompt-cache controls and additional
  Provider-specific content blocks remain incomplete.
- OpenAI-compatible Chat Completions has no portable private-state replay; it
  receives neither another protocol's opaque state nor a fabricated equivalent.
- No live-vendor request was made. Loopback tests prove protocol and recovery
  semantics, not vendor compatibility, model quality or billing.

## References

- Codex `codex-rs/protocol/src/models.rs`
- Codex `codex-rs/core/src/client.rs`
- Codex `codex-rs/core/src/context_manager/history.rs`
- OpenClaw `src/worker/inference-stream.runtime.ts`
- OpenClaw `src/worker/transcript-message.ts`
- OpenClaw `src/agents/transcript-redact.ts`
- `contracts/proto/model_gateway.proto`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/model-gateway/src/{openai_responses,anthropic_messages,lib}.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
