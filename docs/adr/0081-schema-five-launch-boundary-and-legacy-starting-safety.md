# ADR-0081: Schema-five launch boundary and legacy Starting safety

## Status

Accepted and implemented as the ninth process-resource preparation stage. The
production Linux cgroup backend remains fail-closed pending live Linux gates.

ADR-0082 subsequently upgrades the manifest to schema 6 and gives a proven
synchronous `spawn` failure its own durable `start_failed` terminal reason.

## Context

ADR-0080 added durable resource phases, but review of the complete start path
found two unsafe gaps:

- schema 2, schema 3, and the first schema-4 Unix path did not prove which side
  of `spawn` a persisted `Starting` intent had reached;
- a `prepared` Linux group could become empty after a fast Tool executed and
  exited before `Running` was durable.

Treating either state as `RecoveredMissing` can hide a real non-idempotent side
effect and can make an automatic retry appear safe when it is not.

## Decision

1. Process Session Manifest schema 5 defines `prepared` as the durable launch
   boundary for every backend, not only Linux cgroup preparation.
2. Unix and Linux starts must persist `Starting/prepared` before calling
   `spawn`. Publishing `Running/active` is a later, separate operation.
3. Schema-4 manifests are explicitly migrated. Any schema-4 `Starting` state is
   converted to `legacy_unknown`, because the old Unix path cannot prove
   whether `spawn` ran. Non-Starting schema-4 phases retain their meaning.
4. Schema-2 and schema-3 `Starting` states also migrate to `legacy_unknown`,
   independent of backend.
5. A replacement Host may produce `RecoveredMissing` only for a current
   `Starting/unprepared` manifest whose identity is absent. A `prepared` or
   `legacy_unknown` start with an empty or missing identity is persisted as
   `Indeterminate`.
6. If a Linux group is still addressable, reconciliation attempts
   `cgroup.kill=1` even when `populated=0`; the durable result remains
   `Indeterminate` because an earlier side effect cannot be disproved.
7. Active schema-2 Unix sessions remain recoverable: the replacement Manager
   reattaches the original PID/PGID and atomically rewrites the manifest as
   schema 5 `active`. It does not restart the Tool.
8. No legacy or prepared ambiguity path automatically replays a Tool.

## Consequences

### Positive

- A fast Tool cannot be reclassified as “never ran” merely because its process
  and cgroup are already gone at recovery time.
- The same pre-spawn durability rule now applies to Unix and future Linux
  execution.
- Old active Unix sessions retain continuity while ambiguous old starts fail
  safe.
- The recovery decision is based on signed/digested durable state rather than
  timing assumptions about PID or cgroup lifetime.

### Negative and incomplete

- A synchronous `spawn` error currently leaves a durable `Starting/prepared`
  intent for the sweeper to reconcile conservatively; a typed `start_failed`
  terminal transition is not yet implemented.
- Ordinary-directory cgroup fixtures do not prove Linux kernel enforcement or
  `cgroup.kill` completion.
- Real Linux memory/PID/aggregate CPU/kill/cleanup and replacement-Host gates
  remain required before enabling the backend.
- PTY, Windows Job Objects, GUI, Java control plane and cloud/edge deployment
  remain outside this milestone.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/utils/pty/src/process_group.rs` and
  `codex-rs/core/src/exec.rs`
- OpenClaw revision `58b4b9430457`,
  `packages/agent-core/src/harness/env/kill-tree.ts` and
  `src/process/supervisor/supervisor.ts`
- `runtime/crates/tool-runtime/src/process_session.rs`
- `runtime/crates/tool-runtime/tests/process_session_governance.rs`
