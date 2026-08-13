# ADR-0060: Explicit and auditable history import repair

## Status

Accepted and implemented in the protocol crate, Worker and standalone Rust
Host. Repair, model egress and fenced replacement-Host recovery are verified
with a real loopback HTTP exchange and no external control plane.

## Context

Imported or truncated transcripts may contain an Assistant Tool Call without a
result, a Tool Result with no call, a duplicated result, or a result displaced
past later messages. Passing these shapes to providers can fail the request;
silently treating them as authoritative can also replay work or elevate
untrusted content.

Codex `ff352fab6209` normalizes rich `ResponseItem` history by synthesizing
missing outputs and deleting orphan outputs. OpenClaw `58b4b9430457` builds
Tool-use frames, moves uniquely attributable results, drops duplicates/orphans
and inserts synthetic error results. Both apply repair broadly while rebuilding
model context.

## Decision

1. RunExecution schema 15 adds an optional `HistoryImport`. It names either an
   `external` or `truncated` source and carries provider-neutral raw messages.
   Older schemas cannot carry the field.
2. Import and durable subagent history are mutually exclusive in one command;
   their relative ordering would otherwise be ambiguous.
3. Repair is invoked only at this explicit boundary. Checkpoint restore never
   uses it as a fallback for malformed authoritative state.
4. Imported System messages, empty/invalid role content, malformed Tool IDs and
   repeated Tool Call IDs are rejected before model egress. Repeated opaque IDs
   are conservatively ambiguous in the current protocol.
5. A result is moved only to its unique earlier Call. The first valid result is
   kept, later duplicates and results without an earlier owner are dropped, and
   a missing result becomes a Tool-role synthetic error. No historical Tool
   Call enters the execution queue.
6. The report records source kind, source/repaired SHA-256 digests and counts for
   inserted missing, dropped orphan, dropped duplicate and moved results.
7. Worker Checkpoint schema 19 stores the report. Restore recomputes it from the
   replacement command and rejects source or repair drift before model egress.
8. The standalone Host exposes explicit execute/resume methods and returns the
   report in `LocalRunOutcome`. Normal execute/resume paths remain unchanged.

## Consequences

### Positive

- Damaged external history has a deterministic, provider-neutral replay shape.
- Repair cannot grant System authority or accidentally schedule an old Tool.
- Operators can distinguish pristine import, synthetic completion and dropped
  evidence from digest-bound counts.
- The same repaired messages survive a Host replacement exactly.

### Negative and incomplete

- Repeated Tool Call IDs are rejected even though some provider logs reuse
  opaque IDs in later occurrences; OpenClaw handles more of these cases.
- The current local API carries raw imported history again on restore instead
  of persisting a separate immutable Run manifest.
- The optional NATS/control-plane adapter does not yet expose schema 15 import.
- Repair reports are returned and checkpointed but do not yet have a dedicated
  Kernel event type.
- Fork, rollback, branch/reset summaries and provider-private items remain
  unimplemented.

## References

- Codex `codex-rs/core/src/context_manager/{history,normalize}.rs`
- OpenClaw `src/agents/session-transcript-repair.ts`
- OpenClaw `packages/agent-core/src/harness/session/session.ts`
- `contracts/events/run-execution-requested.v15.example.json`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/crates/protocol/tests/history_repair_contract.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
