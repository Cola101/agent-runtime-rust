# Bounded parallel Tool commit evidence — 2026-08-10

## Real execution proof

- A real OpenAI-compatible loopback model returned two `workspace.read_text`
  Tool Calls in one SSE turn.
- The standalone Host launched two real child Tool processes. The slower first
  call and faster second call had overlapping measured process intervals, so
  this is not a mocked scheduler assertion.
- Although the second process completed first, the next HTTP model request
  contained Tool Result messages in the original `call_first`, `call_second`
  order.
- The test interrupted the Host after the second result was staged while the
  first remained unfinished. A replacement Host restored Checkpoint schema 24,
  retried only the unfinished Pure call and submitted both results in source
  order.

## Contract and state proof

- RunExecution schema 17 rejects policy schema 3 and older execution schemas
  reject policy schema 4. `max_concurrent_tools` is bounded to 1–16.
- Worker tests reject a request list reordered after model transport, stage
  out-of-order completion without emitting an event, and recover the exact
  unfinished subset.
- A normal serial Tool result still yields one event. The optional NATS adapter
  now queues every released ordered event and does not request another model
  turn while an ordered batch remains active.
- Existing standalone recovery tests caught and fixed a regression where an
  ephemeral one-shot Run was mistaken for an authoritative root Session merely
  because it carried the v17 branch fence.

## Reference comparison

- Codex allows each Tool implementation to declare parallel support and
  serializes the rest through a shared read/write gate. This Runtime currently
  admits only `Pure` Tools, which is safer but less expressive.
- OpenClaw performs sequential preflight and, absent a sequential barrier, runs
  the whole batch concurrently while later restoring source-order messages.
  This Runtime additionally freezes a signed upper bound and Checkpoints staged
  results, but supports a narrower call set.
- Graphify traced the complete Host path from `drain_tool_calls` through
  `run_approved_tool` into Checkpoint and recovery before implementation. That
  prevented a local `join_all` patch that would have skipped approval and
  durable-start boundaries.

## Validation boundary

- Full Rust workspace: 470 passed, 0 failed, with 5 external live tests
  explicitly ignored; 475 tests total.
- Worker assignment 63, standalone Host 28 and subagent concurrency 20 all
  passed after the execution/checkpoint schema migration.
- Workspace check, Clippy/all-targets/all-features with `-D warnings`, Rust
  formatting, JSON, diff and residue checks passed in the same turn.

No Docker, Java, PostgreSQL, NATS, external daemon or external API key was used.
Loopback HTTP/SSE and real child processes prove Runtime semantics, not
live-vendor compatibility or distributed PubAck recovery.
