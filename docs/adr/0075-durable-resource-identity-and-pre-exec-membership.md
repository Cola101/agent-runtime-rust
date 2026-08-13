# ADR-0075: Durable process-resource identity and pre-exec membership

## Status

Accepted and implemented as the third Linux cgroup v2 preparation stage. The
production `LinuxCgroupV2` backend remains fail-closed because cgroup kill,
cleanup, aggregate CPU supervision and recovery validation are not yet wired.

## Context

ADR-0072 made backend capabilities explicit and ADR-0073 implemented the
controller-file protocol. A replacement Host still had no durable answer to
which resource backend owned a Process Session: schema 2 recorded only PID and
process-group ID. It could not safely validate or reconstruct a cgroup.

Moving a PID into a cgroup after `spawn` is also unsafe. The child can exec,
fork or perform side effects before the parent writes the PID. Membership must
be established in the post-fork child before exec, using a controller file
opened and validated by the parent.

## Decision

1. Process Session Manifest schema 3 stores a tagged `resource_identity`.
   `UnixRlimit` has no external path; `LinuxCgroupV2` stores only the
   deterministic `session-{uuid}` group name, never an arbitrary path.
2. Schema 3 also stores `observed_cpu_usage_micros`, reserved for aggregate
   cgroup CPU supervision. Unix rlimit sessions require this field to remain
   zero so it cannot imply unimplemented accounting.
3. Schema 2 records are verified with their original digest and migrated to a
   Unix rlimit identity. Cgroup cannot be inferred because no older Runtime
   version could activate it. Schema 1 terminal migration remains supported.
4. Cgroup membership is installed as a pre-exec hook. The parent opens
   `cgroup.procs` with final-component symlink protection; the child writes the
   kernel current-process token `0` using only `write(2)` before exec.
5. Group preparation accepts only canonical lowercase UUID group names,
   refuses to adopt a pre-existing group, validates every controller before
   mutation and removes a newly created empty group if setup fails. It never
   recursively removes unexpected contents.
6. The backend selector remains unavailable in production until Manifest
   identity is consumed by spawn, termination, sweep and recovery as one
   lifecycle and is proven on real Linux cgroupfs.

## Consequences

### Positive

- Resource ownership is now part of the digest-protected durable session state.
- The child cannot run user code before the membership hook succeeds.
- A stale or attacker-created group cannot be silently adopted.
- Upgrade does not make signed schema 2 terminal history unreadable.

### Negative and incomplete

- Only schema 2 terminal migration has direct fixture evidence; active schema 2
  migration follows the same code path but still needs replacement-process
  evidence.
- Ordinary-file tests prove pre-exec ordering and exact bytes, not Linux kernel
  controller behavior.
- `cgroup.kill`, `cgroup.events`, aggregate CPU enforcement, terminal cleanup
  and recovery fencing remain missing, so production activation stays blocked.
- Ancestor path replacement still requires fd-relative delegated-root access.

## References

- Linux kernel cgroup v2 documentation:
  <https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html>
- Codex revision `ff352fab6209`,
  `codex-rs/utils/pty/src/process_group.rs` and
  `codex-rs/linux-sandbox/src/linux_run_main.rs`
- OpenClaw revision `58b4b9430457`,
  `src/process/supervisor/adapters/child.ts` and
  `src/process/supervisor/supervisor.ts`
- `runtime/crates/tool-runtime/src/process_resources.rs`
- `runtime/crates/tool-runtime/src/process_session.rs`
