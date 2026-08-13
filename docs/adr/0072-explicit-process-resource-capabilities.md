# ADR-0072: Explicit process resource capabilities before portable backends

## Status

Accepted and implemented as the first phase of the portable process-resource
backend. This ADR does not claim that Linux cgroup v2 or Windows Job Objects are
implemented.

## Context

ADR-0071 added deadline, idle, output, CPU and platform-dependent memory policy.
The remaining risk was architectural rather than cosmetic: a configuration
could ask for a process-count or whole-tree resource guarantee that the current
Unix rlimit backend cannot provide. Accepting such a configuration would make a
policy field look enforced while only the direct child was constrained.

The inspected Codex `ProcessStore` and OpenClaw process supervisor provide
mature lifecycle, timeout and termination behavior, but neither inspected path
is a tenant-bound, persisted whole-tree resource backend. OpenClaw reads cgroup
state for diagnostics; that is not equivalent to placing each Tool session in a
delegated cgroup and enforcing limits there.

## Decision

1. `ProcessSessionResourceCapabilities` is a public, immutable description of
   the backend currently used by process sessions. It separately declares hard
   output-file, CPU-time, memory, process-count and whole-tree accounting.
2. `ProcessSessionResourceBackendKind` currently reports `UnixRlimit` on Unix
   and `Unsupported` elsewhere. A future Linux cgroup backend must introduce a
   new explicit kind; it cannot masquerade as the rlimit backend.
3. `ProcessSessionGovernance` now carries `max_processes` and
   `require_whole_process_tree_accounting`. These are operator requirements,
   not model-supplied Tool arguments.
4. Manager construction validates numeric policy first and then required
   capabilities. An unavailable memory, process-count or whole-tree guarantee
   returns `UnsupportedResourceCapability` before creating the state root,
   launching a Provider or spawning a child.
5. Requirements and the complete capability vector enter the governance
   digest. Since each process Tool implementation digest includes that digest,
   a backend or guarantee change cannot silently reuse an old Tool binding.
6. macOS truthfully advertises hard CPU-time and coarse file-size limits only.
   It does not advertise memory, PID-count or whole-process-tree accounting.

## Consequences

### Positive

- Unsupported limits fail closed at configuration time instead of degrading to
  best effort.
- The next Linux backend has a stable capability contract and cannot be merged
  merely because cgroup-shaped files were written in a unit test.
- Kernel callers can inspect the actual guarantee without depending on GUI,
  Java, Kubernetes, Docker or a control plane.

### Negative and incomplete

- No backend currently satisfies `max_processes` or whole-tree accounting, so
  requesting either intentionally makes the standalone Runtime refuse startup.
- The current Linux rlimit path can constrain per-process address space, but it
  is not aggregate cgroup memory accounting.
- A Linux cgroup v2 backend still needs delegated-root validation, pre-exec
  membership without a spawn race, `memory.max`, `pids.max`, `cpu.stat`,
  `cgroup.kill`, persisted backend identity and real Linux fault tests.
- PTY and GUI remain out of scope.

## References

- Codex `codex-rs/core/src/unified_exec/process_manager.rs`
- OpenClaw `src/process/supervisor/{supervisor.ts,types.ts}`
- Linux kernel `Documentation/admin-guide/cgroup-v2.rst`
- `runtime/crates/tool-runtime/src/process_session.rs`
- `runtime/crates/tool-runtime/tests/process_resource_capabilities.rs`
