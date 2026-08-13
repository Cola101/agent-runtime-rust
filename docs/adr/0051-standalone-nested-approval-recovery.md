# ADR-0051: standalone nested approval routing and recovery

## Status

Accepted and implemented for child Tool approvals, parent IPC routing,
decision idempotency and process-crash recovery. Parallel child supervision is
specified by ADR-0052; other approval kinds remain follow-up work.

## Context

ADR-0049 and ADR-0050 made child Runs independently durable and recoverable,
but a child Tool approval could only stop inside the recursive child Host. The
root daemon had no durable target identity for that approval. A decision also
had a second crash window: the child could consume it and advance its
Checkpoint while the root `run.json` still said `ApprovalDecided`; a replacement
would then attempt to apply the same decision to a child that no longer waited.

Codex `ff352fab6209dc0f9d13fc0036ed3f9404682b2c` filters delegated approval
events from ordinary child output and routes exec, patch, user-input,
permission and compatible MCP decisions through the parent Session. OpenClaw
`58b4b9430457e91b44f0ccce73ad1b6c6bb11e28` does not currently project child
approval events to ancestor streams; its operator-approval refactor proposes a
durable SQLite lifecycle, authenticated audience projections and first-answer
CAS, while explicitly excluding blocked Tool resumption across Gateway restart.

## Decision

1. Every pending local approval carries the exact `target_run_id` in addition
   to its approval ID and Tool binding digest. The root daemon persists that
   target in `AwaitingApproval`; absent targets are accepted only for legacy
   root approvals.
2. IPC writes `ApprovalDecided` atomically before acknowledging a reviewer. The
   state binds target Run, approval ID, binding digest and decision. Repeating
   the same decision is idempotent; a different decision is rejected.
3. Recovery may carry a decision through a parent only while its Checkpoint says
   `WaitForSubagent`. Each recursive Host either forwards it to that exact child
   or rejects a mismatched target. A decision never falls through to model work.
4. A child whose Checkpoint still says `WaitForApproval` rebinds the approval to
   the restored attempt, verifies ID and digest, and applies the decision.
5. Applying a decision records the minimal durable receipt in the Worker
   Checkpoint: approval ID, binding digest and allow/deny value. Full transport
   commands are not persisted because attempt and Worker identities change on
   restore.
6. If a later Checkpoint has already left `WaitForApproval`, recovery skips
   replay only when that exact receipt is present. A missing receipt, changed
   digest, changed decision or wrong Run fails closed.
7. Denial remains a model-visible bound Tool error and never exposes an
   execution request. Allow-once executes the approved Tool at most once across
   the tested process-crash window.

## Consequences

### Positive

- A root client can review a child Tool without knowing the child transport.
- Approval acknowledgement, child consumption and parent completion may each be
  separated by a daemon crash without losing the decision or replaying the Tool.
- The implementation remains filesystem-only and protocol/provider neutral.
- Exact target, approval and digest binding prevents a decision from being
  redirected to another child or another Tool request.

### Negative

- Only Tool `allow_once` and `deny` decisions are routed. Codex also handles
  patch, user-input, permission and MCP elicitation variants.
- There is no authenticated multi-reviewer surface or first-answer database CAS;
  local IPC is still a single-machine authority.
- The recursive Host has no independent wait/send/close API. ADR-0052 adds an
  eight-child blocking batch, not long-lived child interaction.

## References

- ADR-0049: standalone role subagent execution
- ADR-0050: standalone child cancellation and recovery
- `runtime/apps/runtime-host/tests/subagent_approval.rs`
- `runtime/apps/runtime-host/src/{lib,ipc}.rs`
- `runtime/apps/worker/src/lib.rs`
