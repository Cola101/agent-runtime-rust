# Generation-fenced subagent Rollback evidence — 2026-08-09

## Behavioural proof

- RED: a valid `agent.rollback` model call on a terminal two-turn handle failed
  because the Worker did not activate that control Tool.
- GREEN: planning succeeds only for the caller-observed generation and an older
  completed boundary with no active or queued child work.
- RED: after Worker planning existed, the standalone Host reached the call and
  failed with `no tool executor is installed for agent.rollback`.
- GREEN: loopback HTTP drives parent spawn, child trusted
  `workspace.read_text`, a second child turn, rollback, archived/current history
  reads, generation-2 send and a successful parent terminal result.
- The stable handle moves from generation 1 history `[0, 1]` to generation 2
  head `[0]`; its next completed turn produces `[0, 2]`. Generation 1 remains
  byte-equivalent and readable after the new branch appends.
- Across all child Runs only one trusted Tool execution starts. The retained
  Assistant Tool Call/Result pair appears once in generation-2 model context
  but is never scheduled again.

## Fencing and recovery proof

- A generation-1 command is rejected after rollback. A generation-1 late
  result cannot settle a generation-2 active request because its binding digest
  differs.
- Worker crash-window proof checkpoints the rollback record before the parent
  Tool result. A replacement Worker returns the same receipt and event ID and
  does not increment to generation 3.
- Checkpoint schema 21 stores each superseded Turn once plus immutable ordinal
  heads. Mutating an archived result while recomputing the outer Checkpoint
  digest is still rejected by the historical generation digest.
- Archive count/size and generation count are bounded. Unreferenced archive
  Turns, missing heads, malformed paths and provenance drift fail closed.

## Reference comparison

- Codex `ff352fab6209` remains ahead on root Thread rollback, persisted rollout
  replay, cumulative markers, reference-context reconstruction and arbitrary
  `num_turns` removal.
- OpenClaw `58b4b9430457` remains ahead on Gateway Session reset, active-run and
  queue cleanup, transcript archive files, lifecycle ownership and product hook
  integration.
- This Runtime is narrower, but stable-handle generation fencing, immutable
  per-generation reads and digest-checked deduplicated history are explicit
  local multi-tenant safety properties rather than claims of overall parity.

## Evidence boundary

The model peer is deterministic loopback HTTP, while the OpenAI-compatible
adapter, parent/child Agent loops, trusted native Tool, filesystem Checkpoint,
event log and replacement Worker are production paths. This does not prove a
third-party provider or a general root Session rollback API.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used.

## Validation

- Targeted Worker rollback, archive integrity and crash-window recovery: passed.
- Worker assignment suite: 58 passed, 0 failed.
- Targeted standalone Host Tool-backed rollback loop: passed.
- Host subagent concurrency suite: 19 passed, 0 failed on the final rerun.
- Standalone Host suite: 22 passed, 0 failed.
- Full workspace and static quality gates are recorded after the final gate.
