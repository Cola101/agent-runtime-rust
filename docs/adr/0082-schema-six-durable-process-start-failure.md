# ADR-0082: Schema-six durable process start failure

## Status

Accepted and implemented as the tenth process-resource stage. The production
Linux cgroup backend remains fail-closed pending live Linux gates.

ADR-0083 subsequently closes the typed ToolExecutor/Worker/Host propagation
gap recorded below without changing schema 6.

## Context

ADR-0081 made `prepared` a cross-backend pre-spawn boundary. The remaining
synchronous failure branch still removed prepared resources and returned an I/O
error without changing the durable Manifest. A replacement Host therefore saw
`Starting/prepared` and correctly, but unnecessarily, classified the session as
`Indeterminate` even though `Command::spawn` had synchronously proved that the
target program was never published.

Adding a new `start_failed` termination reason to schema 5 would also break
schema-version truthfulness: an older schema-5 reader does not know that enum
value.

## Decision

1. Process Session Manifest schema 6 adds `start_failed` to the durable
   termination reasons.
2. `ProcessSessionError::StartFailed` returns the failed Session ID and the
   operating-system reason to the direct Manager caller.
3. If `Command::spawn` returns an error after the durable `prepared` record,
   the Manager first persists `Terminated/start_failed`, clears unpublished
   PID/input identity, and advances the operation sequence.
4. Resource cleanup runs only after the terminal record is durable. Unix is
   immediately `cleaned`; a Linux cleanup failure leaves `cleanup_pending` for
   a replacement Manager to retry.
5. Schema-5 records are digest-verified and migrated to schema 6. A real active
   schema-5 process can be reattached and rewritten without being restarted.
6. A crash before the terminal record is durable remains conservatively
   `Indeterminate`; no path automatically replays the non-idempotent Tool.

## Consequences

### Positive

- A proven synchronous launch failure no longer consumes active-session quota
  or masquerades as an ambiguous side effect.
- Audit state distinguishes “program never started” from “program may have run.”
- Cleanup failure does not erase the known launch outcome and remains
  recoverable by another Host.
- Schema-5 active sessions preserve continuity during upgrade.

### Negative and incomplete

- At this stage, `ProcessSessionToolExecutor` still flattened the typed Manager
  error into `PersistentProcessSession(String)`. ADR-0083 subsequently closed
  that propagation gap and preserves `start_failed` as a distinct safe code.
- A storage failure while writing the terminal record still leaves
  `Starting/prepared`; recovery deliberately treats it as indeterminate.
- Ordinary-directory cgroup fixtures do not prove Linux kernel enforcement or
  cleanup completion.
- Real Linux, PTY and Windows Job Object gates remain incomplete.

## References

- Codex revision `ff352fab6209`, `codex-rs/core/src/{exec,spawn}.rs` and
  `codex-rs/utils/pty/src/process_group.rs`
- OpenClaw revision `58b4b9430457`,
  `src/process/supervisor/supervisor.ts` and
  `packages/agent-core/src/harness/env/kill-tree.ts`
- `runtime/crates/tool-runtime/src/process_session.rs`
- `runtime/apps/runtime-host/tests/subagent_approval.rs`
