# Protocol-neutral rich model item evidence — 2026-08-10

## Provider protocol proof

- A real loopback OpenAI Responses SSE stream returned a reasoning item with
  summary, id and encrypted content. The next request to the same route replayed
  the exact typed item and requested `reasoning.encrypted_content`.
- Selecting another route removed the encrypted state before HTTP egress and
  emitted a bounded omission event without opaque data. A 429 after that audit
  event still reached the frozen fallback, proving audit is not committed output.
- A real loopback Anthropic Messages stream returned `thinking_delta` and
  `signature_delta`. The Runtime retained both as private state, never emitted
  thinking as text, and reconstructed the same thinking block on the next call.
- OpenAI refusal delta/done became one typed refusal. The standalone Host
  returned the refusal text instead of a blank successful output.

## Transport and recovery proof

- Protobuf/gRPC round-tripped reasoning, refusal and private-state omission as
  distinct model events.
- Worker Checkpoint replacement onto a new attempt retained summary and exact
  private-state provenance/data in the next ModelInvocation.
- Context compaction removed an older prefix while preserving a recent
  reasoning + Tool Call + Tool Result tail; a replacement Worker reconstructed
  byte-equivalent messages.
- A standalone root Session completed four real HTTP turns. Continue, Fork and
  Rollback all replayed the reasoning item from the immutable source Turn to the
  same route.

## Reference comparison

- Codex parity improved for typed Responses reasoning, encrypted continuation
  and durable history. Codex still has broader Responses item coverage,
  WebSocket transport and mature rollout compatibility.
- OpenClaw parity improved for thinking/signature retention and display
  separation. OpenClaw still has broader Provider-specific cache, Auth Profile,
  OAuth and content compatibility.
- This Runtime adds an explicit route/protocol/model/format provenance fence and
  an audit-only omission event. That difference is deliberate for multi-tenant,
  multi-Provider execution; it is not evidence of broader Provider parity.

## Validation

- Full Rust workspace: 463 passed, 0 failed, with 5 external live tests
  explicitly ignored; 468 tests total.
- `cargo check --workspace --all-targets` passed before the full test run.
- Clippy over workspace/all-targets/all-features with `-D warnings`, Rust
  formatting, diff and residue checks passed in the same implementation turn.

No Docker, Java, PostgreSQL, NATS, external daemon or external API key was
started. Loopback peers used real HTTP/SSE and gRPC sockets. The generated
Graphify analysis directory was removed after it informed the end-to-end item
path; Rust `target` was retained as reusable build cache.
