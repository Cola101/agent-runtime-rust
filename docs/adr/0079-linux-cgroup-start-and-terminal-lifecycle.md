# ADR-0079: Linux cgroup start and terminal lifecycle

## Status

Accepted and implemented as the seventh Linux cgroup v2 preparation stage. The
production backend remains fail-closed until crash-window reconciliation and
real Linux enforcement are proven.

## Context

ADR-0078 pinned one delegated-root identity for the manager lifetime, but the
actual `process.start` path still spawned a child without preparing a cgroup or
installing membership. Terminal manifests also returned immediately from a
sweep, so an empty cgroup left by a crashed cleanup task was never retried.

## Decision

1. Persist the `Starting` intent and complete ordinary command setup before
   creating a Linux cgroup. Then prepare/configure the deterministic
   `session-{uuid}` group through the manager-owned root handle.
2. Open `cgroup.procs` from the prepared group and install the `0` membership
   write in the child's pre-exec path. A child cannot execute the Tool program
   before that write succeeds.
3. If controller preparation, membership installation or `spawn` fails before
   a child exists, drop the group handle and remove the group relative to the
   pinned root. Cleanup failure is fail-closed as `Indeterminate`.
4. Terminal group removal uses `unlinkat(..., AT_REMOVEDIR)` relative to the
   manager-owned root. Missing groups are success, making cleanup idempotent;
   non-empty/populated groups remain protected by the kernel's directory
   removal semantics.
5. Publish the terminal process state before attempting group removal. The
   watcher, explicit close and governance termination attempt cleanup
   immediately, while every later terminal sweep retries it after a Host crash.
6. This does not activate Linux cgroups. Public construction still rejects the
   backend, and ordinary-file tests do not prove cgroupfs or kernel enforcement.

## Consequences

### Positive

- The future Linux path can no longer execute a Tool outside its declared
  cgroup merely because start orchestration omitted the membership hook.
- Pre-spawn failures do not leave an empty cgroup behind.
- Terminal cleanup survives manager/Host replacement and cannot be redirected
  by replacing the configured delegated-root path.
- Unix rlimit behavior and the standalone Mac Runtime remain unchanged.

### Negative and incomplete

- ADR-0080 subsequently distinguishes missing groups from ambiguous controller
  state, persists the start/cleanup resource phase, and quarantines possible
  post-spawn work as `Indeterminate` without automatic replay.
- Active schema 2 replacement and real Linux memory/PID/aggregate CPU/kill/
  cleanup/Host-replacement evidence remain missing.
- Windows Job Objects and PTY lifecycle remain outside this milestone.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/utils/pty/src/process_group.rs` and
  `codex-rs/core/src/exec.rs`
- OpenClaw revision `58b4b9430457`,
  `packages/agent-core/src/harness/env/kill-tree.ts` and
  `src/process/supervisor/supervisor.ts`
- `runtime/crates/tool-runtime/src/process_resources.rs`
- `runtime/crates/tool-runtime/src/process_session.rs`
