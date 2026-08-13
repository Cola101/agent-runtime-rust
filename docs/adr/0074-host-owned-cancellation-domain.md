# ADR-0074: Host-owned cancellation domain closes detached subagent work

## Status

Accepted and implemented.

## Context

An asynchronous subagent task is represented by a Tokio `JoinHandle` plus a
cancellation token. Dropping a `JoinHandle` detaches rather than cancels its
task, and dropping a `CancellationToken` does not signal descendants. A Host
execution that panicked, was aborted or was otherwise dropped without calling
the async `shutdown` method could therefore leave child model requests and TCP
connections running without an owner.

The old recovery test usually hid this defect by destroying a nested Tokio
Runtime after aborting the Host. Under a full workspace run, the loopback
Provider once observed the old parent or child socket remain open beyond five
seconds. A successful retry was not evidence that ownership was correct.

## Decision

1. `LocalRuntimeHost::start_with_cancellation` derives an internal child token
   from the caller token. The Host owns this descendant domain and can cancel
   it without cancelling sibling components that share the caller token.
2. `Drop` cancels the Host token, cancels every registered subagent token and
   aborts every still-registered task. This is the synchronous safety net for
   unwinding and task abortion.
3. Explicit `shutdown` cancels the same Host domain before awaiting subagent
   completion and stdio MCP cleanup. Graceful shutdown remains preferable;
   `Drop` prevents detached work when graceful shutdown is impossible.
4. The recovery test keeps the Tokio Runtime alive, aborts and awaits the Host
   execution task, and requires the real loopback Provider to observe both
   parent and child TCP sockets close before a replacement Host starts.
5. Durable Checkpoint semantics do not change. The replacement restores the
   same asynchronous handle and must not replay `agent.spawn`.

## Consequences

### Positive

- Host ownership now has an explicit terminal action on both graceful and
  abnormal destruction paths.
- The test proves connection closure comes from Host cleanup rather than from
  process-wide Runtime destruction.
- Caller cancellation still flows downward, while Host destruction cannot
  cancel unrelated caller-owned siblings.

### Negative and incomplete

- `Drop` cannot await cooperative cleanup, so it aborts registered tasks after
  signalling cancellation. Code that requires graceful protocol shutdown must
  still call `shutdown`.
- This proof covers local HTTP model connections and async subagents. It does
  not replace Linux process-tree, cgroup, remote MCP or operating-system crash
  evidence.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/core/src/client_common.rs` (`ResponseStream::drop` cancels its
  upstream consumer) and `session/handlers.rs` (explicit process cleanup)
- OpenClaw revision `58b4b9430457`,
  `src/process/supervisor/supervisor.ts` (cancel, TERM-to-KILL and adapter
  disposal)
- `runtime/apps/runtime-host/src/lib.rs`
- `runtime/apps/runtime-host/tests/subagent_concurrency.rs`
