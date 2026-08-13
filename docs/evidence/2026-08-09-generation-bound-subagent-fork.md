# Generation-bound subagent Fork evidence — 2026-08-09

## Behavioural proof

- RED: a valid `agent.fork` model Tool call failed planning because the Worker
  exposed no such control capability.
- GREEN: the Worker plans an idempotent control Tool only for an existing
  completed activation boundary and a non-increasing finite budget.
- RED: the standalone Host reached the planned call and failed because no Fork
  executor existed.
- GREEN: real loopback HTTP drives parent spawn, child model, trusted
  `workspace.read_text`, parent Fork, forked `agent.send`, fork child model and
  both `agent.history` reads to a successful parent terminal result.
- The source history remains one turn. The fork begins with the byte-equivalent
  selected turn, then appends its own second turn under a different stable
  handle and generation 1.
- The fork child request contains the source Assistant Tool Call and bound Tool
  Result exactly once. Across both child Runs only one trusted Tool execution
  starts, proving the historical Tool was not replayed.
- Worker crash-window proof checkpoints after the Fork record but before its
  Tool result. A replacement Worker returns the same handle, receipt and event
  ID and reconstructs byte-equivalent source and fork histories.

## Authority and recovery proof

- The caller must bind source generation 1; stale generations fail planning.
- The fork role is copied, not caller-selectable, so scopes cannot increase.
- The requested budget is below the source cap and is checked again against the
  parent Run's remaining token, cost and active-time budget during execution.
- Checkpoint schema 20 stores generation indexes and Fork provenance. Legacy
  checkpoints derive generation 1 only when no v20 branch state is present.
- Fork copies no active request, queued message, message receipt, close marker
  or process task. The branch starts terminal at its selected history head and
  gains live state only after a new `agent.send`.

## Reference comparison

- Codex `ff352fab6209` remains ahead on general Thread Fork, latest/through/before
  boundaries, paginated lineage, retention leases and app-server lifecycle.
- OpenClaw `58b4b9430457` remains ahead on Gateway Session integration,
  context-engine preparation and product-level visible/isolated fork policies.
- This Runtime is narrower but explicitly binds tenant Run authority, source
  generation, budget and Checkpoint identity. That is a local multi-tenant
  safety advantage, not overall feature parity.

## Evidence boundary

The model peer is deterministic loopback HTTP, but the OpenAI-compatible
adapter, parent and child Agent loops, trusted native Tool, filesystem events,
Checkpoint and replacement Worker are production paths. This does not prove a
third-party provider or a complete root Session Fork API.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used.

## Validation

- Targeted Worker Fork and recovery test: passed.
- Targeted standalone Host Tool-backed Fork loop: passed.
- Worker assignment suite: 57 passed, 0 failed.
- Host subagent concurrency suite: 18 passed, 0 failed.
- Full Rust workspace: 430 passed, 0 failed, 5 explicitly ignored external live
  tests; 435 listed tests in total.
- `cargo check --workspace --all-targets`, Clippy for
  workspace/all-targets/all-features with `-D warnings`, Rust formatting, JSON
  parsing and `git diff --check` all passed.
- No runtime Host, trusted Tool, subagent-concurrency test or temporary Tool
  directory remained after validation. `runtime/target` was retained as build
  cache.
