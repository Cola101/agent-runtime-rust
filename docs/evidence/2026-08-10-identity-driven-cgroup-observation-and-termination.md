# Identity-driven cgroup observation and termination evidence — 2026-08-10

## TDD proof

- Three controller tests first failed because CPU/populated readers and the
  kill writer did not exist. Minimal no-op functions then produced behavioral
  RED: CPU returned zero, duplicate populated state was accepted and
  `cgroup.kill` remained empty.
- The implemented readers use real file descriptors with final-component
  symlink protection. The tests now observe literal `usage_usec 42001`, both
  populated states, rejection of duplicate state and an unchanged external
  symlink target. The kill controller receives exactly `1\n`.
- A separate Manifest test first failed with zero observed CPU. It now reads
  the schema 3 identity's `cpu.stat`, persists `2000000` microseconds and
  returns the explicit `cpu_limit` governance decision at two seconds.
- The first version of that test exposed an invalid fixture because it omitted
  the real `control.lock`; adding the production-side lock file made the test
  exercise the same atomic Manifest mutation boundary instead of bypassing it.

## Real Process Session regression

- `cargo test -p agent-tool-runtime --all-targets --quiet` passed all 64 tests.
- These tests include real child processes, process groups, replacement-manager
  reattach, deadline/idle/output enforcement, crash sweeping, quota admission
  and the standalone Tool Process Session lifecycle on macOS.
- The Unix path now passes its frozen backend identity through start
  supervision, interact, recover and sweep without changing public behavior.

## Full validation

- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- A serial full gate, `cargo test --workspace --all-targets --quiet --
  --test-threads=1`, passed 511 tests with zero failures; five external live
  tests remained explicitly ignored. `cargo test -- --list` confirmed 516
  total tests.
- The first default-parallel full run was interrupted after the existing
  `a_new_host_recovers_the_same_async_handle_without_replaying_spawn` test made
  no progress for several minutes. Follow-up diagnosis proved its crash driver
  could abort before the Provider established the sockets the test expected to
  observe closing. The test now has an explicit connection-ready edge and
  bounded phases; 20 repeated parallel runs and a default-parallel full gate
  pass. See `2026-08-10-deterministic-host-crash-gate.md`.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, virtual machine or external
  Provider was started.

## Reference comparison

- Codex `ff352fab6209` has mature PTY and process-group TERM/KILL paths plus
  background-child tests. The inspected path does not expose a durable
  tenant-bound cgroup identity or aggregate CPU ledger.
- OpenClaw `58b4b9430457` has broader cross-platform process adapters,
  overall/no-output timeout and TERM-to-KILL escalation. The inspected
  supervisor registry remains process-local and does not persist this cgroup
  lifecycle.
- This Runtime is stricter only in its narrow digest-bound resource identity
  and replacement-Host accounting contract. It remains behind both projects
  in terminal/PTY breadth and behind OpenClaw in cross-platform supervision.

## Validation boundary

- Controller tests use ordinary files on macOS. They prove byte protocol,
  parsing, symlink rejection and Manifest decisions, not Linux kernel behavior.
- Production selection still returns `linux_cgroup_v2_backend_not_wired`.
- fd-relative delegated-root containment, terminal group removal, active
  schema 2 replacement, delegated controller setup and live memory/PID/CPU/kill
  pressure tests remain unverified.
