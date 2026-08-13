# ADR-0058: Protocol-neutral context compaction

## Status

Accepted and implemented in the protocol-neutral Worker and standalone Rust
Host. Complete Tool Call/Result boundaries, summary authority, budget charging,
Checkpoint restore and a failed-summary Host replacement are behaviourally
verified without an external control plane or service.

## Context

The Worker already persisted model messages, but long Runs could only grow
until a hard history bound failed closed. A safe compactor cannot flatten the
history to text: assistant narrative, Tool calls and bound Tool results must
remain typed, a cut must never leave an orphan call/result, and a summary must
not acquire system-message authority.

Codex `ff352fab6209` rewrites versioned rich `ResponseItem` history, normalizes
missing/orphan Tool outputs, and supports local and remote model-generated
compaction. OpenClaw `58b4b9430457` persists a compaction boundary and
`firstKeptEntryId`, retains a token-budgeted tail, repairs Tool pairing, strips
runtime-only Tool details before summarization and supports iterative/branched
summaries.

## Decision

1. RunExecution schema 13 and runtime-policy schema 3 carry an immutable,
   provider-neutral compaction policy. Compaction is disabled by default. When
   enabled, trigger bytes, retained bytes and maximum summary tokens are all
   bounded and `retain_bytes < trigger_bytes` is mandatory.
2. The Worker transcript keeps typed `ModelMessage` values. Assistant text
   emitted before a Tool call is stored in the same assistant message as that
   call; the bound result remains a Tool-role message with the original call ID.
3. Compaction preserves every leading System message verbatim. It selects the
   oldest prefix whose removed and retained sides both contain complete Tool
   Call/Result sets. If no such boundary exists, it does not compact.
4. Summarization uses the ordinary Model IR and the Run's frozen provider
   authority, but exposes no Tools or output schema. The request contains a
   dedicated summarizer System instruction, the typed source prefix and one
   final User instruction.
5. The generated summary re-enters normal context as an ordinary User message
   prefixed with `[Earlier conversation summary]`. It cannot replace or extend
   Agent/Skill System instructions.
6. Worker Checkpoint schema 17 stores pending and applied compaction records.
   SHA-256 bindings cover Run/Session/Tenant/model-policy identity, the full
   source transcript, removed prefix, retained tail, counts and the exact
   compaction policy. Restore rejects drift or a schema 13 Run backed by an
   older Checkpoint.
7. The standalone Host persists the pending boundary before provider egress.
   A replacement Host rebuilds the same summary request and never replays Tool
   calls already represented by bound results.
8. Summary model usage is charged to the same Run budget. Applying a summary
   emits `context.compacted` with source, summary and retained-tail digests; a
   budget terminal prevents another ordinary model turn.
9. Repeated compaction is suppressed until new transcript messages exist.

## Consequences

### Positive

- The same Kernel semantics work with OpenAI-compatible, Responses, Anthropic
  or future adapters; no provider wire type enters the Checkpoint.
- Recent Tool evidence stays exact while old context becomes bounded lower-
  authority text.
- A provider failure after preparation has a deterministic, auditable recovery
  path and cannot move the compaction boundary.
- The full path runs locally without Docker, Java, PostgreSQL, NATS or a model
  gateway process.

### Negative and incomplete

- Trigger/retention use encoded IR bytes, not a provider tokenizer or context-
  window capability profile. This is deterministic but less space-efficient.
- Summarization is currently one bounded request. There is no multi-chunk plan,
  large-message placeholder, summary quality evaluation or rollback.
- Unlike OpenClaw, the compactor does not yet strip provider/runtime-specific
  Tool detail fields because the current IR stores only model-visible results.
  Future richer result metadata must remain outside the summarizer projection.
- Unlike Codex, there is no remote-compaction path, complete rollout item set,
  fork/full/last-N history mode or history rollback.
- Stable subagent history still carries completed question/answer turns rather
  than each child Run's internal Tool/reasoning/multimodal transcript.
- Local event-log append and Checkpoint replacement are separate filesystem
  writes; cross-file atomic crash reconciliation remains a later durability gap.

## References

- Codex `codex-rs/core/src/context_manager/{history,normalize}.rs`
- Codex `codex-rs/core/src/compact.rs`
- OpenClaw `packages/agent-core/src/harness/session/session.ts`
- OpenClaw `packages/agent-core/src/harness/compaction/compaction.ts`
- OpenClaw `src/agents/compaction-planning.ts`
- `contracts/events/run-execution-requested.v13.example.json`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/worker/tests/assignment.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
