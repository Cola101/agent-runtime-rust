# Persistent subagent mailbox and interrupt evidence — 2026-08-09

## Behavioural proof

- RED: a real model Tool call containing `interrupt=true` was rejected as an
  unknown field. After implementation, the same HTTP/SSE flow closes the old
  child stream, starts the redirect under the stable `agent_id`, waits it and
  finishes the parent.
- A running child accepts an ordinary message as `queued`. The old stream must
  complete before `subagent.input.activated` appears and the queued child model
  request starts. The final Checkpoint holds an empty FIFO mailbox.
- Duplicate input during an active turn replays the same submission receipt and
  does not create another child connection.
- RED: a schema 15 queue referencing a missing receipt restored successfully.
  Restore now rejects missing/duplicate/wrong-status mailbox state. A synthetic
  schema 14 receipt without the new fields still restores and is normalized to
  its actual active state.
- The crash test receives the durable `subagent.input.accepted` interrupt event,
  confirms the Checkpoint receipt is still `queued`, and destroys the first
  Tokio runtime. A new Host settles the interrupt before ordinary recovery,
  never sends the old input to the provider, executes the redirect once and
  records exactly 150 child Tokens.

## Reference comparison

- Codex `ff352fab6209` remains ahead on a resident Thread, rich input items and
  complete history. Its tested ordering is Interrupt then UserInput; this
  Runtime adds a caller-keyed Checkpoint receipt before cancellation.
- OpenClaw `58b4b9430457` remains ahead on steering/follow-up modes, generation
  operations, delivery lifecycle and long-lived Session governance. Its queue
  split informed the ordinary-versus-urgent distinction.
- This Runtime is stronger only on the narrow standalone recovery invariant:
  an interrupt acknowledgement and its replacement request survive Host loss
  without restarting the redirected old input.

## Evidence boundary

Tests use real local HTTP/SSE sockets, cancellation-driven connection closure,
filesystem Checkpoints and an actually destroyed Tokio runtime. The provider is
deterministic, so the evidence proves Runtime ordering/recovery rather than
vendor model quality or provider exactly-once behavior. No Docker, Java,
PostgreSQL, NATS or external service is involved.

## Validation

- Related Kernel, Worker and Host packages pass, including 15/15 subagent
  concurrency tests and 54/54 Worker assignment tests.
- Runtime workspace: 413 passed, 0 failed and 5 explicitly ignored external
  live cases; 418 test items listed.
- Clippy workspace/all-targets/all-features with `-D warnings`, Rust formatting
  and `git diff --check` passed.
