# ADR-0080: Durable process resource phase and Starting reconciliation

## Status

Accepted and implemented as the eighth Linux cgroup v2 preparation stage. The
production backend remains fail-closed until live Linux enforcement and
replacement-Host recovery are proven.

ADR-0081 subsequently upgrades the manifest to schema 5, makes `prepared` a
cross-backend pre-spawn boundary, and treats all schema-2/3/4 `Starting` states
as legacy-ambiguous rather than assuming they stopped before spawn.

## Context

ADR-0079 wired cgroup preparation into `process.start` and retried terminal
cleanup after Host replacement. It still left three ambiguous states:

- a `Starting` manifest did not say whether the cgroup had been prepared;
- a replacement Host could not distinguish an absent group from a malformed or
  unreadable controller;
- terminal cleanup had no durable pending/completed marker.

These ambiguities are unsafe for a non-idempotent Tool. A process may have
executed after spawn even though the `Running` manifest was never committed, so
the replacement Host must not silently retry it.

## Decision

1. Process Session Manifest schema 4 persists a digest-bound resource phase:
   `unprepared`, `prepared`, `active`, `cleanup_pending`, `cleaned`, or
   `legacy_unknown`. State/backend/phase combinations are validated and invalid
   combinations fail closed.
2. Linux start persists `unprepared`, prepares the deterministic group, then
   persists `prepared` before spawn. A successful spawn persists `active`
   together with `Running`. Preparation, membership, spawn, or intermediate
   persistence failure rolls the group back through the pinned root.
3. Terminal Linux states first persist `cleanup_pending`; successful
   root-relative removal then persists `cleaned`. Unix terminal states persist
   `cleaned` directly. Repeated cleanup is idempotent.
4. A missing session-group directory is a typed `GroupMissing` result. It is
   different from malformed, unreadable, or contradictory controller data.
5. A replacement Host reconciles `Starting` as follows:
   - no process identity and no group: `RecoveredMissing`, then `cleaned`;
   - populated prepared group: write `cgroup.kill=1`, persist `Indeterminate`
     and `cleanup_pending`;
   - ambiguous controller state: attempt the same quarantine kill and persist
     `Indeterminate`; never infer that the Tool did not run.
6. Schema 1, 2, and 3 remain readable through explicit migration. A schema-3
   Linux `Starting` manifest becomes `legacy_unknown`; it is never upgraded to
   a falsely precise phase.
7. No `Starting` reconciliation path automatically retries the Tool. Existing
   reconciliation policy for non-idempotent or unknown side effects remains
   authoritative.

## Consequences

### Positive

- Replacement Hosts have a durable start/cleanup journal instead of inferring
  lifecycle solely from a process state and path existence.
- A clean pre-spawn crash can terminate deterministically, while a possible
  post-spawn crash is quarantined and remains explicitly indeterminate.
- Missing resources and corrupted controller evidence no longer collapse into
  the same recovery result.
- Terminal cleanup completion survives another Host replacement.

### Negative and incomplete

- Ordinary-directory fixtures do not prove kernel cgroup membership, pressure,
  `cgroup.kill` completion, or non-empty-group removal behavior.
- Direct schema-3 migration and active schema-2 replacement fixtures remain to
  be added even though migration code is present and the workspace gate passes.
- Real Linux memory/PID/aggregate CPU/kill/cleanup and Host replacement remain
  unverified, so production backend selection still fails closed.
- Windows Job Objects, PTY, GUI, Java control plane and cloud/edge deployment
  remain outside this milestone.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/utils/pty/src/process_group.rs` and
  `codex-rs/core/src/exec.rs`
- OpenClaw revision `58b4b9430457`,
  `packages/agent-core/src/harness/env/kill-tree.ts` and
  `src/process/supervisor/supervisor.ts`
- `runtime/crates/tool-runtime/src/process_resources.rs`
- `runtime/crates/tool-runtime/src/process_session.rs`
