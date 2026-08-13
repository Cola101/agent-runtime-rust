# ADR-0071: Persistent process session governance and recoverable supervision

## Status

Accepted and implemented for the standalone Rust Runtime. Deadline, idle,
admission and process-resource behavior is verified with real child processes
on macOS. Linux address-space enforcement is implemented but is not claimed as
live-verified by this ADR.

## Context

ADR-0070 made a Tool process durable across Host replacement, but its original
64-session cap did not distinguish tenants or Workspaces and a Host restart
could not be allowed to reset time or resource budgets.

Codex keeps at most 64 entries in its process-local `ProcessStore`, tracks
`last_used`, bounds returned output and prunes old entries. That is effective
for one interactive client, but evicting a live process when capacity is full
is not an acceptable multi-tenant admission policy. OpenClaw's process
supervisor has overall/no-output timeouts, capped capture, explicit cancellation
reasons and TERM-to-KILL escalation, but the inspected registry is likewise
process-local. This Runtime needs those lifecycle controls to survive Host
replacement without losing tenant ownership or refreshing a budget.

## Decision

1. `ProcessSessionGovernance` is operator configuration, not a model argument.
   It freezes global, per-tenant and per-Workspace active limits; maximum
   runtime; idle timeout; per-stream output size; CPU seconds; and an optional
   memory limit. Its digest is part of the Tool implementation binding.
2. Manifest schema 2 persists the governance digest, absolute execution
   deadline, idle duration, last activity, resource ceilings, observed output
   sizes and a typed termination reason. A replacement Host uses those values;
   it never derives fresh deadlines from its local clock or defaults.
3. A cross-process capacity lock serializes count and publication. Admission is
   fail-closed at global, tenant and `(tenant, canonical Workspace)` scopes.
   Malformed or incomplete state consumes global capacity; capacity pressure
   never silently terminates another tenant's live process.
4. stdin writes and stdout/stderr growth count as activity. A read-only poll
   does not keep a process alive. The per-session supervisor sleeps until the
   nearest deadline, bounded to one second, rather than scanning every session
   on a fixed high-frequency timer.
5. The child receives hard `RLIMIT_CPU` and `RLIMIT_FSIZE` limits before exec.
   `RLIMIT_FSIZE` is intentionally a coarse process-wide file-size boundary,
   not a stdout-only primitive. On supported non-macOS Unix targets an optional
   `RLIMIT_AS` memory ceiling is installed. macOS rejects an explicit memory
   ceiling because its address/data limits failed at exec in live testing; the
   default is `None` rather than pretending the limit exists.
6. A per-session sweep lock serializes deadline/identity inspection, durable
   termination intent, TERM-to-KILL escalation and terminal publication.
   `interact`, `recover` and the Host Agent Loop sweep before acting. Any
   ambiguous identity makes the Host fail closed instead of signalling a PID.
7. Schema-1 terminal history may be digest-verified and migrated read-only.
   A live schema-1 session is `indeterminate`, because no evidence proves that
   its process was launched with schema-2 resource limits.
8. The entire path remains local-file and OS-process based. It has no required
   Java, PostgreSQL, NATS, Docker, Kubernetes or external Provider dependency.

## Consequences

### Positive

- A Host restart cannot refresh execution, idle, output, CPU or memory policy.
- Noisy or abandoned sessions converge to typed terminal reasons, and quota is
  attributed to the correct tenant and Workspace.
- The admission rule is deterministic across replacement Hosts and does not
  use Codex-style live LRU eviction as a multi-tenant shortcut.
- The supervisor cost scales with deadline activity rather than a 25 ms global
  polling loop.

### Negative and incomplete

- `RLIMIT_FSIZE` also limits files opened by the Tool process. A future Linux
  backend should use cgroup accounting and dedicated output transport for
  finer attribution.
- macOS has verified CPU and file-size limits but no verified memory hard
  limit. Linux `RLIMIT_AS` exists in code but still needs a Linux live gate;
  Windows needs a Job Object backend.
- A surviving Host supervises its children and a replacement Host can sweep
  persisted orphans. If every Host is absent, there is deliberately no hidden
  system daemon; cleanup resumes when the standalone Runtime is started.
- PTY resize, pause/resume, binary output frames and full-screen terminal
  applications remain unsupported.

## References

- Codex `codex-rs/core/src/unified_exec/{mod.rs,process_manager.rs}`
- OpenClaw `src/process/supervisor/{supervisor.ts,types.ts}`
- `runtime/crates/tool-runtime/src/process_session.rs`
- `runtime/crates/tool-runtime/tests/process_session_governance.rs`
- `runtime/crates/tool-runtime/tests/process_session_sweeper_crash.rs`
- `runtime/apps/runtime-host/tests/process_session_loop.rs`
