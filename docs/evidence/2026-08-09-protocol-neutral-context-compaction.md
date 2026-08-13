# Protocol-neutral context compaction evidence — 2026-08-09

## Behavioural proof

- RED: after two real HTTP MCP Tool turns, the standalone Host made only three
  provider requests. The third response was treated as the final answer because
  the Worker's compaction state was not connected to the Host loop.
- GREEN: the same Run now makes four requests. Requests one and two select and
  execute two real MCP Tools. Request three has no Tools, caps output at 256
  tokens and contains the old typed assistant call/result prefix. Request four
  contains an ordinary User summary plus the exact recent assistant narrative,
  Tool call ID and Tool result; the old 2,500-byte result is absent.
- The summary text is not appended to user-visible output. `context.compacted`
  is persisted before the final `run.succeeded` path.
- A second real test returns HTTP 503 for the first summary request after the
  pending boundary is Checkpointed. A fresh Host restores on a new attempt,
  sends byte-equivalent messages and limits for the summary request, applies it,
  and completes the Run. The MCP Tool call counter remains exactly two, proving
  recovery did not replay either completed Tool.
- Worker-level proof checks source/retained counts, summary User authority,
  complete recent Tool pairing, digest-bound application, budget charging and
  exact post-compaction transcript equality after a fenced Checkpoint restore.
- RunExecution v13 publishes runtime-policy v3 in a concrete JSON contract.
  Downgrading it to execution v12, policy v2 or a non-shrinking retention policy
  is rejected.

## Reference comparison

- Codex `ff352fab6209` remains ahead on rich rollout items, normalization that
  can synthesize missing Tool output/remove orphan output, automatic/manual and
  remote compaction, fork modes, rollback and history-version lifecycle.
- OpenClaw `58b4b9430457` remains ahead on tokenizer-informed planning,
  multi-part summaries, previous-summary updates, oversized-message handling,
  runtime-detail stripping, branch/reset boundaries and Session tree replay.
- This Runtime now matches the core summary-plus-safe-tail behavior and has a
  narrower deterministic recovery property: source/prefix/tail/policy digests
  freeze the exact boundary before provider egress. That is useful for fenced
  multi-tenant execution, but it does not make the overall history system more
  capable than either reference.

## Evidence boundary

The tests use real loopback HTTP/SSE, the production OpenAI-compatible adapter,
a real Streamable HTTP MCP server, Kernel events and filesystem Checkpoints.
The model peer is deterministic, so the result proves protocol conversion,
Tool transcript integrity, compaction ordering and Host replacement semantics;
it does not prove third-party summary quality, provider exactly-once delivery,
tokenizer accuracy or long-session behavior.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used. Tests
abort their loopback tasks and temporary state/workspace directories are owned
by `tempfile`.

## Validation

- Standalone Host: 20 passed, 0 failed.
- Worker assignment: 55 passed, 0 failed.
- Protocol crate: 62 passed, 0 failed; execution contract subset 32 passed.
- Runtime workspace: 418 passed, 0 failed and 5 explicitly ignored external
  live cases; 423 test items listed.
- Clippy workspace/all-targets/all-features with `-D warnings`, Rust formatting,
  JSON contract parsing and `git diff --check` passed.
- No test-owned Runtime/Tool process, listening port or `agent-tool-runtime-*`
  temporary directory remained. The reusable Cargo `target` cache was retained
  and was not counted as a runtime artifact.
