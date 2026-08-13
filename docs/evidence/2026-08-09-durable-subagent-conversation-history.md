# Durable subagent conversation history evidence — 2026-08-09

## Behavioural proof

- RED: the second child HTTP request contained only the current follow-up user
  message. The passing request now contains exactly the initial user input, the
  first terminal assistant text and the follow-up user input in user/assistant/
  user roles. System instructions remain separate.
- The same real model loop calls `agent.history` after two child turns. Its Tool
  result exposes both completed turns, activation ordinals and terminal output,
  then the parent model completes normally.
- A confirmed follow-up is crashed while its provider stream is active by
  destroying the first Tokio runtime. The replacement Host reconstructs the
  child request with the exact three-message conversation and completes it once.
- An interrupt test proves acceptance sequence and activation order are distinct:
  the cancelled initial turn is ordinal 0 and the redirect is ordinal 1. Two
  one-item history pages return those turns without duplication.
- Schema 16 restore rejects a forged activation sequence. Existing mailbox
  corruption tests continue rejecting missing receipt and status mismatches.
  A synthetic schema 14 receipt remains recoverable, with an explicitly empty
  history because that schema never stored one.
- RunExecution v12 accepts well-formed child history and rejects the same field
  when downgraded to v11.

## Reference comparison

- Codex `ff352fab6209` remains ahead: `send_input` targets a resident Thread,
  V2 agents reload history, fork can inherit full or last-N context, and
  compaction/reconstruction retains richer Tool and reasoning items.
- OpenClaw `58b4b9430457` remains ahead: `Agent` owns a mutable transcript,
  steering/follow-up queues drain into it, and Session context understands
  compaction/reset boundaries and richer persisted message types.
- This Runtime now closes the largest semantic hole in ADR-0056 and is stronger
  only on a narrow recovery property: the history prefix, interrupt ordering and
  activation binding are independently Checkpointed and rejected on drift.

## Evidence boundary

Tests use real local HTTP/SSE sockets, the production OpenAI-compatible adapter,
filesystem events and Checkpoints, a read-only Tool call, cancellation-driven
connection closure and an actually destroyed Tokio runtime. The model peer is
deterministic, so this proves Runtime protocol, ordering and recovery semantics,
not vendor model quality or provider exactly-once delivery. No Docker, Java,
PostgreSQL, NATS or external service is involved.

The inherited capsule contains completed handle-level question/answer turns. It
does not yet prove full internal Tool transcript carry-over, compaction quality,
multimodal history or a long-duration third-party provider Session.

## Validation

- Worker assignment: 54 passed, 0 failed.
- Protocol execution contract: 31 passed, 0 failed.
- Standalone Host subagent concurrency/recovery: 15 passed, 0 failed.
- Runtime workspace: 414 passed, 0 failed and 5 explicitly ignored external
  live cases; 419 test items listed.
- Clippy workspace/all-targets/all-features with `-D warnings`, Rust formatting
  and `git diff --check` passed.
