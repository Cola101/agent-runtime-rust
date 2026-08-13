# ADR-0077: FD-relative cgroup lifecycle boundary

## Status

Accepted and implemented as the fifth Linux cgroup v2 preparation stage. The
production backend remains fail-closed until terminal cleanup and real Linux
enforcement are proven. Manager-lifetime root pinning is completed by ADR-0078.

## Context

ADR-0076 made the persisted cgroup identity drive CPU accounting, liveness and
termination, but those operations still reconstructed controller paths from a
`PathBuf`. Protecting only the final controller component with `O_NOFOLLOW`
does not stop a delegated root or cgroup directory from being renamed and
replaced between operations.

## Decision

1. Open the configured delegated root as a directory descriptor with
   `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` and treat the descriptor, not its
   pathname, as the authority for subsequent operations.
2. Accept only canonical `session-{uuid}` group names. Open child groups with
   `openat` and `O_DIRECTORY | O_NOFOLLOW`; open every controller relative to
   that group descriptor.
3. Create groups with `mkdirat`. On failed preparation, drop the group handle
   and use `unlinkat(..., AT_REMOVEDIR)` beneath the same root descriptor.
   Existing groups are never adopted and rollback is never recursive.
4. Pre-open all limit controllers before the first write. Membership,
   `cpu.stat`, `cgroup.events` and `cgroup.kill` all use the same fd-relative
   group API.
5. A termination operation opens the group once and holds that descriptor
   across the kill-and-wait sequence. The old path-based lifecycle entry points
   are removed so future callers cannot silently bypass containment.
6. This does not activate Linux cgroups. The public manager still rejects the
   backend, and ordinary-file tests do not prove cgroupfs delegation or kernel
   enforcement.

## Consequences

### Positive

- Renaming and replacing either the delegated-root pathname or the group
  pathname cannot redirect an in-flight create, configure, membership,
  observation, kill or rollback operation.
- Controller symlinks remain rejected and all mutations stay under the opened
  authority boundary.
- The macOS Unix process-session path remains unchanged and keeps the Runtime
  independent from Docker and external services.

### Negative and incomplete

- Manager-lifetime root identity is addressed by ADR-0078.
- Successful terminal-group removal is not wired into the supervisor.
- No real Linux cgroupfs test has exercised memory, PID, aggregate CPU,
  `cgroup.kill`, delegation or Host replacement.
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
