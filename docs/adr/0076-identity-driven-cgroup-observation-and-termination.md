# ADR-0076: Identity-driven cgroup observation and termination

## Status

Accepted and implemented as the fourth Linux cgroup v2 preparation stage. The
production `LinuxCgroupV2` backend remains fail-closed. ADR-0077 closes the
in-flight fd-relative operation boundary; manager-lifetime root pinning,
terminal group cleanup and real Linux enforcement remain.

## Context

ADR-0075 persisted a deterministic resource identity and installed membership
before exec. The supervisor still ignored that identity after spawn: liveness
used only the Unix process group, CPU governance never consumed `cpu.stat`, and
termination never used `cgroup.kill`. Activating the backend in that state
would let a replacement Host misclassify descendants or enforce only the
leader's CPU limit.

## Decision

1. Controller reads use an already-existing ordinary controller file and
   `O_NOFOLLOW`; `cpu.stat` must contain exactly one `usage_usec` and
   `cgroup.events` exactly one `populated` value of `0` or `1`.
2. A schema 3 Linux identity resolves only to the deterministic
   `session-{uuid}` child of the configured delegated root. A manager whose
   backend identity differs from the Manifest refuses access.
3. Every governance sweep refreshes aggregate CPU usage into the digest-bound
   Manifest. Usage must be monotonic. Reaching `max_cpu_seconds` produces the
   explicit durable termination reason `cpu_limit`.
4. Unix sessions continue to use process-group liveness and TERM-to-KILL.
   Linux cgroup sessions use `cgroup.events: populated` for liveness and write
   `1` to `cgroup.kill` for whole-group termination, then wait for both an empty
   cgroup and release of the inherited identity lease.
5. Start supervision, interaction, recovery and manual sweeping all receive
   the manager's frozen backend configuration; none infer a backend from the
   host platform or from an arbitrary persisted path.
6. Production activation remains blocked. Path joins are not yet fd-relative,
   a terminal empty cgroup is not yet removed, active schema 2 replacement has
   no process-level proof, and ordinary files cannot prove kernel enforcement.

## Consequences

### Positive

- Resource identity now affects supervision and termination rather than being
  write-only Manifest metadata.
- CPU budget applies to the cgroup's aggregate process tree counter.
- Replacement supervision can use a kernel-owned populated bit instead of
  treating the leader PID as the whole session.
- Existing macOS Unix process-session behavior remains unchanged and continues
  to run without Docker or an external control plane.

### Negative and incomplete

- The Linux backend is still unavailable through the public constructor.
- `cgroup.kill` has exact-byte and symlink tests, but no live cgroupfs test.
- Terminal group removal, manager-lifetime root pinning,
  delegated-controller negotiation and Linux memory/PID/CPU pressure tests
  remain. In-flight path replacement resistance is covered by ADR-0077.
- Windows Job Objects and PTY lifecycle are outside the current milestone.

## References

- Linux kernel cgroup v2 documentation:
  <https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html>
- Codex revision `ff352fab6209`,
  `codex-rs/utils/pty/src/process_group.rs` and
  `codex-rs/core/src/exec.rs`
- OpenClaw revision `58b4b9430457`,
  `src/process/supervisor/supervisor.ts` and
  `src/process/supervisor/adapters/child.ts`
- `runtime/crates/tool-runtime/src/process_resources.rs`
- `runtime/crates/tool-runtime/src/process_session.rs`
