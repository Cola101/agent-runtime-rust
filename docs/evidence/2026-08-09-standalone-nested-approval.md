# Standalone nested approval evidence — 2026-08-09

## Scope

This evidence covers one real parent → child Tool → parent IPC approval → child
resume → parent completion loop, including crashes before and after decision
consumption. It does not claim parallel child supervision, authenticated remote
reviewers, multi-surface CAS or non-Tool approval kinds.

## Behavioural proof

- `a_child_tool_approval_routes_through_the_parent_and_survives_a_daemon_restart`
  runs the shipped trusted workspace Tool, a real Unix IPC daemon, filesystem
  Checkpoints and a deterministic HTTP model peer.
- The first daemon parks the child on `approval.required`; no
  `tool.execution.started` event exists before the root client approves.
- The second daemon records the exact `ApprovalDecided` binding before IPC
  acknowledgement. The same allow decision is accepted idempotently and an
  immediate conflicting deny decision is rejected.
- After the child executes the Tool and writes a Checkpoint with no pending
  approval, the test drops the second Tokio runtime before the parent can
  finish. A third daemon resumes the same child and parent identities.
- The original RED ended as `failed: ... tool approval identity or binding does
  not match the reviewed request`: recovery tried to apply an already-consumed
  decision. The Worker Checkpoint now carries the exact applied-decision receipt.
- The recovered child continues its model turn, returns the bound result to the
  parent, and the parent succeeds. `tool.execution.started` and
  `subagent.result.received` each occur exactly once.
- `a_child_approval_bound_to_another_run_never_executes_the_tool` changes the
  durable target Run before resolution. Recovery terminates fail-closed and the
  real child has no `tool.execution.started` event.

## Source comparison used

- Codex `ff352fab6209`: `core/src/codex_delegate.rs` routes delegated exec,
  patch, request-user-input, request-permissions and compatible MCP approvals
  through the parent Session and does not expose them as ordinary child events.
- OpenClaw `58b4b9430457`: current subagent ownership metadata does not project
  approvals to ancestor streams. `docs/refactor/operator-approvals.md` proposes
  durable first-answer CAS and audience projections but names blocked Tool
  execution resumption across Gateway restart as a non-goal.

## Validation

- Targeted nested-approval tests: 2 passed, 0 failed.
- Host tests: 47 passed, 0 failed. The existing denial flow and Worker binding
  mismatch tests passed alongside the new crash-recovery loop.
- Full workspace: 386 passed, 0 failed, 5 explicitly ignored live integration
  cases; 391 test items listed.
- Workspace Clippy with all targets/features under `-D warnings`, formatting and
  `git diff --check`: passed.
- Residue audit found no Runtime/Tool process, Unix socket, partial file or Tool
  test temporary directory. The reusable Rust `target` cache was retained.
