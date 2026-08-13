# ADR-0073: Linux cgroup v2 protocol boundary before backend activation

## Status

Accepted. The controller-file protocol is implemented and unit-tested, but the
`LinuxCgroupV2` backend remains deliberately unavailable on every platform.
This ADR does not claim Linux cgroup enforcement.

## Context

ADR-0072 made resource guarantees explicit and fail-closed. The next risk is a
partial Linux implementation: writing `memory.max` in isolation is not enough
when process membership, recovery identity, aggregate CPU accounting, kill and
cleanup are not one durable lifecycle.

Linux cgroup v2 exposes process migration through `cgroup.procs`, hard memory
and PID limits through `memory.max` and `pids.max`, aggregate CPU usage through
`cpu.stat usage_usec`, and whole-cgroup termination through `cgroup.kill`.
Those files form one kernel protocol, not independent optional hints.

## Decision

1. `ProcessSessionResourceBackendConfig` explicitly distinguishes the existing
   `UnixRlimit` backend from a configured `LinuxCgroupV2 { delegated_root }`.
2. Selecting Linux cgroup v2 on a non-Linux host returns
   `UnsupportedPlatform` before the state root is created. A Linux build also
   returns `linux_cgroup_v2_backend_not_wired` until the complete lifecycle is
   connected; partial enforcement cannot be activated accidentally.
3. The internal protocol writes exact newline-terminated values for
   `memory.max`, `memory.oom.group`, `pids.max`, `cgroup.max.depth` and
   `cgroup.max.descendants`. Every final controller path is validated before
   the first mutation, files are never created, and Unix opens use
   `O_NOFOLLOW | O_CLOEXEC`.
4. Child membership uses the kernel current-process token `0` through a
   syscall-only writer suitable for a future pre-exec hook. `cpu.stat` parsing
   accepts exactly one numeric `usage_usec` field and rejects missing,
   duplicate or malformed values.
5. Backend configuration and the resolved capability vector enter the
   governance digest. The manager stores the resolved vector instead of
   recomputing it after construction.
6. Activation requires a schema revision that persists cgroup identity,
   pre-exec membership without a spawn race, supervisor CPU reads,
   `cgroup.kill`, terminal cleanup, recovery and real Linux fault evidence.

## Consequences

### Positive

- The filesystem and parsing boundary can be reviewed independently while the
  public Runtime still refuses an incomplete backend.
- Symlink controller substitution is rejected without modifying its target.
- Backend selection is explicit and cannot silently fall back to rlimit.

### Negative and incomplete

- Unit tests use ordinary files. They prove byte protocol and path safety, not
  kernel cgroup behavior, delegation or controller availability.
- The delegated directory path itself still needs an fd-based or `openat2`
  containment design; `O_NOFOLLOW` protects final file components but does not
  prevent an ancestor directory replacement race.
- No Linux target or host was available, so compile and live enforcement are
  unverified.
- PTY, Windows Job Objects, GUI and control-plane integration remain out of
  scope.

## References

- Linux kernel cgroup v2 documentation:
  <https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html>
- Codex revision `ff352fab6209`,
  `codex-rs/core/src/unified_exec/process_manager.rs`
- OpenClaw revision `58b4b9430457`,
  `src/process/supervisor/supervisor.ts`
- `runtime/crates/tool-runtime/src/process_resources.rs`
- `runtime/crates/tool-runtime/src/process_session.rs`
- `runtime/crates/tool-runtime/tests/process_resource_capabilities.rs`
