# Durable resource identity and pre-exec membership evidence — 2026-08-10

## TDD proof

- The first real Process Session test closed its child before asserting and
  then failed because the durable Manifest reported schema 2 instead of 3.
  After implementation it records literal
  `{ "kind": "unix_rlimit" }` and zero observed cgroup CPU usage.
- A separately signed schema 2 terminal fixture initially failed with
  `Indeterminate`. Explicit digest verification and v2 migration made the same
  history readable without weakening malformed-state rejection.
- The pre-exec test first compiled only with a no-op hook and then failed
  because `cgroup.procs` remained empty. The real child now exits successfully
  only after its pre-exec hook writes `0\n`; all 7 cgroup protocol cases pass.
- Group preparation tests prove a pre-existing group remains unchanged and a
  newly created empty group is removed when required controllers do not appear.

## Real Process Session closure

- Persistent Process Session suite: 7/7 passed, including replacement-manager
  reattach/write/read/close against the original PID and the new durable
  resource identity assertion.
- Governance and migration suite: 8/8 passed.
- Resource capability suite: 5/5 passed.
- Owner-process crash and replacement sweeper: 1/1 passed against a real child.
- Standalone Host Agent Loop process-session suite: 3/3 passed.

## Full validation

- `cargo test --workspace --all-targets --quiet`: 507 passed, 0 failed and 5
  external live tests explicitly ignored, 512 total.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.
- No Docker, Java, PostgreSQL, NATS, Kubernetes or external Provider was used.

## Reference comparison

- Codex `ff352fab6209` uses pre-exec process-group setup and has mature PTY,
  termination and sandbox process paths. The inspected ProcessStore path does
  not persist a tenant Process Session cgroup identity.
- OpenClaw `58b4b9430457` has detached-child/process-tree handling plus
  supervisor TERM-to-KILL escalation and remains broader across platforms. The
  inspected supervisor registry is process-local rather than a durable cgroup
  identity ledger.
- This Runtime is stricter only in the digest-bound multi-tenant identity and
  refusal to adopt an existing group. Kill/recovery breadth still trails both.

## Validation boundary

- The child and pre-exec hook were real macOS processes, but `cgroup.procs` was
  an ordinary test file. No Linux target or cgroupfs was available.
- Production Linux cgroup selection still fails closed with
  `linux_cgroup_v2_backend_not_wired`.
- cgroup kill, populated-state validation, aggregate CPU enforcement, cleanup
  and active schema 2 replacement recovery remain unverified.
