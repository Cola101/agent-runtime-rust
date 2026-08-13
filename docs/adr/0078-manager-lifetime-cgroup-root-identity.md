# ADR-0078: Manager-lifetime cgroup root identity

## Status

Accepted and implemented as the sixth Linux cgroup v2 preparation stage. The
production backend remains fail-closed until start/cleanup lifecycle wiring and
real Linux enforcement are proven.

## Context

ADR-0077 made each cgroup operation descriptor-relative, but the process
session manager still reopened the configured delegated-root pathname for every
operation. Replacing that pathname between sweeps could therefore redirect a
long-lived manager to a different cgroup hierarchy even though each individual
operation was internally safe.

## Decision

1. Keep the public backend configuration immutable and serializable, but resolve
   it once into a private runtime backend when the manager is constructed.
2. For Linux cgroup v2, open the delegated root once and retain the resulting
   `LinuxCgroupV2Root` in an `Arc`. Watchers, supervisors and foreground manager
   operations receive clones of that resolved handle rather than the configured
   pathname.
3. Derive resource identities and perform group open, CPU observation,
   liveness, attachment and termination only through the resolved backend.
   Operational functions no longer accept the public path configuration.
4. Keep capability resolution and governance validation before backend opening.
   The public Linux backend therefore remains fail-closed on unsupported hosts
   and still returns `linux_cgroup_v2_backend_not_wired` on Linux.
5. A task may retain its `Arc` after the manager value is dropped. This is
   intentional: the manager-owned cancellation domain determines task
   lifetime, while every surviving task stays pinned to the same root identity.

## Consequences

### Positive

- Renaming and replacing the delegated-root pathname after manager creation can
  no longer redirect later sweeps or termination operations.
- The type boundary makes accidental per-operation pathname re-resolution a
  compile-time design error rather than a test convention.
- The macOS Unix rlimit backend remains lightweight and independent of Docker,
  Java or external services.

### Negative and incomplete

- Linux process start still does not create/configure a group and install
  membership through this handle in the public manager path.
- Successful terminal empty-group removal is not wired into the supervisor.
- Active schema 2 recovery still needs an explicit replacement policy.
- No real Linux cgroupfs test has exercised delegation, memory, PID, aggregate
  CPU, kill, cleanup or replacement-Host behavior.
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
