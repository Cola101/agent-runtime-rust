# Subagent transcript capsule evidence — 2026-08-09

## Behavioural proof

- RED: after a child used a real trusted `workspace.read_text` Tool, the next
  `agent.send` request contained only flattened User/Assistant text and did not
  contain the prior Assistant Tool call or bound Tool result.
- GREEN: the same loopback HTTP model now observes, in order, the original User
  input, Assistant narrative and Tool Call, Tool Result, final Assistant and the
  follow-up User message. The trusted Tool executes once.
- RED: a terminal child attempt could not be checkpointed, and publishing a
  child terminal event left the latest Checkpoint in `Running` state.
- GREEN: Checkpoint schema 18 accepts terminal attempts, stores the exact typed
  transcript and is written before the standalone Host exposes a terminal child
  event.
- A deterministic crash-window test captures the parent before its result
  receipt, lets the child complete and persist its terminal Checkpoint, then
  replaces the Host. Recovery reconstructs the rich result from that Checkpoint;
  the follow-up request retains the exact Tool pair and does not replay the Tool.
- Protocol tests prove digest binding, reject a missing Tool result, reject an
  orphan Tool result, require rich history in RunExecution v14 and reject schema
  downgrade carrying the new state.
- The first full-workspace gate exposed that terminal Checkpoint persistence had
  also overwritten root Runs' last resumable snapshot. Recovery tests for plain
  local, HTTP MCP and stdio MCP failed. The ordering rule is now limited to
  delegated child Runs; all three root recovery paths pass again.

## Reference comparison

- Codex `ff352fab6209` remains ahead on full `ResponseItem` coverage, generic
  normalize/repair, history version lifecycle, fork modes and rollback.
- OpenClaw `58b4b9430457` remains ahead on provider-specific transcript repair,
  rich Session entry compatibility, branch/reset/compaction boundaries and
  Session-tree replay.
- This Runtime now matches the core typed Tool-history requirement for stable
  subagents. Its narrow additional property is deterministic standalone recovery
  from a terminal child Checkpoint before the parent receipt exists. That is not
  a claim of overall parity or cloud exactly-once delivery.

## Evidence boundary

The tests use the production OpenAI-compatible HTTP conversion path, real
loopback HTTP, a real trusted native Tool process, Kernel events and filesystem
Checkpoints. The model peer is deterministic, so this proves message conversion,
Tool pairing, ordering and Host replacement semantics; it does not prove a
third-party provider, private reasoning items, multimodal content, imported
history repair or long-session quality.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used. Test
servers, Tool processes and temporary workspaces are test-owned.

## Validation

- Standalone Host subagent concurrency: 17 passed, 0 failed.
- Worker assignment: 56 passed, 0 failed.
- Protocol execution contract: 33 passed, 0 failed.
- Protocol subagent recovery contract: 4 passed, 0 failed.
- Protocol crate: 64 test items.
- Runtime workspace: 423 passed, 0 failed and 5 explicitly ignored external
  live cases; 428 test items listed.
- Clippy workspace/all-targets/all-features with `-D warnings`, Rust formatting,
  all JSON contracts and `git diff --check` passed.
- No test-owned Runtime/Tool process, matching listening port or
  `agent-tool-runtime-*` temporary directory remained. The reusable Cargo
  `target` cache was retained.
