# Checkpoint-first subagent message receipt evidence — 2026-08-09

## Behavioural proof

- RED: once the real Host fixture added `idempotency_key`, the old Runtime
  rejected `agent.send` as malformed. The model-visible schema now requires the
  key and the same fixture completes.
- The real HTTP lifecycle sends the same key and exact message twice while the
  successor child stream is active. Both calls return the same
  `agent_id:sequence` submission ID; only one child connection and one durable
  receipt are created.
- Worker replacement restores schema 14, replays the identical receipt without
  a second acceptance event, and rejects the same key with different content
  as `SubagentMessageConflict`.
- The crash test confirms a send, starts both the parent follow-up request and a
  streaming successor child, then destroys the first Tokio runtime. A new Host
  eagerly restores the active request before any `wait`, resumes the same
  logical child Run and finishes the parent. The final Checkpoint contains one
  receipt and the event log contains one `subagent.input.accepted`.
- Existing closed-handle recovery still rejects a new idempotency key after
  close, while an exact replay can only return its old receipt and cannot
  resurrect the child.

## Reference comparison

- Codex `ff352fab6209` remains ahead on resident Thread input, rich items,
  interrupt and full conversation history. Its handler returns submission ID,
  but the inspected boundary is not a caller-keyed durable receipt.
- OpenClaw `58b4b9430457` remains ahead on completion outbox retries,
  generations, delivery suppression and kill reconciliation. Its durable
  delivery machinery informed the state split, but is broader than
  `sessions_send` input acceptance.
- This Runtime is stronger only on the narrow standalone invariant: an exposed
  input acknowledgement is backed by a content-bound Checkpoint receipt.

## Evidence boundary

The test uses real local HTTP/SSE sockets, a streaming child, filesystem
Checkpoint replacement and a destroyed Tokio runtime. Deterministic provider
responses prove Runtime delivery semantics, not vendor model quality or
provider exactly-once execution. No Docker, Java, PostgreSQL, NATS or external
service was started.

## Validation

- Runtime workspace: 409 passed, 0 failed, 5 explicitly ignored external live
  cases; 414 test items listed.
- Related protocol, Kernel, Worker and Host packages passed together;
  subagent concurrency passed 12/12 and Worker assignment passed 53/53.
- Clippy: workspace/all targets/all features with `-D warnings` passed.
- Rust formatting and `git diff --check` passed.
