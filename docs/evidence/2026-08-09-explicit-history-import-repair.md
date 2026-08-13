# Explicit history import repair evidence — 2026-08-09

## Behavioural proof

- RED: the protocol had no explicit history-import type or repair API.
- GREEN: a provider-neutral repair now moves one uniquely attributable displaced
  result, inserts one missing result, drops orphan and duplicate results, and
  reports each change with source/repaired digests.
- System authority injection and repeated Tool Call IDs fail before model
  egress; the repairer does not guess an ambiguous owner.
- RED: the standalone Host had no execute/resume path for imported history.
- GREEN: a real loopback OpenAI-compatible request receives repaired User,
  Assistant Tool Call, synthetic Tool Result and later User messages in exact
  order. No `tool.execution.started` event is produced.
- A replacement Host submits the same raw import, restores Checkpoint schema 19
  and emits a byte-equivalent model message array. Changing the imported User
  content makes restore fail with Checkpoint identity mismatch before any new
  provider request.

## Reference comparison

- Codex `ff352fab6209` remains ahead on rich item coverage and automatic
  normalization throughout rollout reconstruction.
- OpenClaw `58b4b9430457` remains ahead on occurrence-based repeated ID handling,
  provider-specific repair and Session-tree branch/reset lifecycle.
- This Runtime is intentionally narrower: repair is an explicit lower-authority
  import operation, while durable Runtime state remains fenced. The source and
  repaired digests make this boundary more suitable for multi-tenant audit, but
  do not establish overall parity.

## Evidence boundary

The integration tests use the production OpenAI-compatible adapter, real
loopback HTTP, Worker admission, filesystem Checkpoints and replacement-Host
restore. The deterministic peer proves protocol shape, ordering and absence of
Tool replay; it does not prove third-party provider tolerance or quality.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used.

## Validation

- Protocol history repair: 2 passed, 0 failed.
- Protocol execution contract: 34 passed, 0 failed.
- Standalone Host: 22 passed, 0 failed.
- Worker assignment: 56 passed, 0 failed.
- Full Rust workspace: 428 passed, 0 failed, 5 explicitly ignored external live
  tests; 433 listed tests in total.
- `cargo check --workspace --all-targets`, Clippy for
  workspace/all-targets/all-features with `-D warnings`, Rust formatting, JSON
  parsing and `git diff --check` all passed.
- No runtime Host, trusted Tool, standalone test or history-repair process,
  listener or `agent-tool-runtime-*` temporary directory remained after the
  validation run. `runtime/target` was retained as build cache.
