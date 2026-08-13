# Persistent asynchronous subagent handle evidence — 2026-08-09

## Behavioural proof

- The spawn RED proved the old Host waited for child completion. The real HTTP
  fixture now observes the parent receive a stable `agent_id` while the child
  stream is still open; a short `wait` times out without cancellation and a
  later `wait` receives the terminal result.
- The close RED proved the control Tool was absent. The Host now cancels only
  the selected child, observes the real socket close, emits
  `subagent.closed`, and persists the closed set in Checkpoint schema 13.
- The recovery test destroys the first Tokio runtime after the parent handle
  and child Checkpoint are durable. A new Host restores the same `agent_id`,
  does not replay spawn, and lazily resumes the child from its Checkpoint.
- The send RED proved the control Tool was absent. A completed child now
  accepts one sequenced follow-up, creates a deterministic successor child Run,
  and returns the successor result through the same stable handle.
- The cleanup RED left a child connection alive after the parent succeeded.
  Parent terminalization now cancels and awaits unclosed asynchronous tasks.
- `a_closed_async_subagent_handle_cannot_be_resurrected_by_send_after_recovery`
  proves the irreversible close edge survives Worker replacement and rejects a
  later send.

ADR-0055 subsequently adds schema 14 idempotency receipts and the confirmed
send crash boundary; this file remains the evidence for the first handle slice.

## Reference comparison

- Codex `ff352fab6209` is still ahead on resident Thread history, live input,
  interrupt, status subscriptions and general session recovery.
- OpenClaw `58b4b9430457` is still ahead on its durable operational registry,
  delivery retry, generation ownership, pause/timeout state and kill
  reconciliation.
- This Runtime now has a narrower but explicit Checkpoint-first identity,
  budget, close and crash boundary suitable for a standalone protocol-neutral
  kernel. That does not establish feature parity.

## Evidence boundary

Tests use real local HTTP/SSE sockets, separate child Runs, filesystem
Checkpoints, cancellation tokens and Host replacement. Provider responses are
deterministic fixtures, so the evidence proves Runtime lifecycle semantics, not
vendor model reasoning quality. No Java, Docker, PostgreSQL, NATS or external
service was started.

## Validation

- Runtime workspace: 407 passed, 0 failed, 5 explicitly ignored external live
  cases; 412 test items listed.
- Related protocol, Kernel, Worker and Host packages passed in one complete
  package run; the subagent concurrency binary passed 11/11 and Worker
  assignment passed 52/52.
- Clippy: workspace/all targets/all features with `-D warnings` passed.
- Rust formatting and `git diff --check` passed.
