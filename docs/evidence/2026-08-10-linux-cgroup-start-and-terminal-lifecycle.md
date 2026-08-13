# Linux cgroup start and terminal lifecycle evidence — 2026-08-10

## TDD proof

- The start-path RED constructed the private Linux backend and executed a real
  trusted native Tool. It returned `Running` with a real PID, proving that
  `process.start` bypassed cgroup preparation entirely.
- The GREEN path prepares the deterministic group through the manager-owned
  root after ordinary command setup, installs pre-exec membership and refuses
  to spawn when the ordinary-file fixture cannot expose cgroup controllers.
  The child marker is absent and the failed group is removed.
- The terminal cleanup API first failed to compile because no root-relative,
  idempotent removal operation existed. It now removes an empty original group
  twice successfully while leaving a replacement-path group untouched.
- The terminal-sweep behavioral RED returned `Terminal` but left the original
  group behind. It now retries cleanup through the manager-owned root and does
  not touch the replacement configured path.
- Existing tests continue to prove exact limit writes, pre-exec `0` membership,
  preparation rollback, CPU/populated observation and `cgroup.kill` semantics.

## Full validation

- `agent-tool-runtime` passed all 72 tests under default parallelism; its
  all-features Clippy gate passed with warnings denied.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo test --workspace --all-targets --quiet` passed under default
  parallelism. The inventory contains 524 tests: 519 executed and passed, zero
  failed, and five external live tests remained explicitly ignored.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, virtual machine, external
  Provider or API key was used. No test process or matching temporary directory
  remained after the gate.

## Reference comparison

- Codex `ff352fab6209` has mature process-group setup, parent-death signaling,
  TERM/KILL escalation, PTY integration and Linux sandbox product paths. The
  inspected helpers do not provide this persisted tenant/session cgroup
  lifecycle or replacement-Host cleanup retry.
- OpenClaw `58b4b9430457` has broader Unix/Windows tree kill, group-leader
  validation, timeout arbitration and adapter supervision. Its inspected
  supervisor registry is process-local rather than a durable cgroup ledger.
- This Runtime is stricter only in the narrow digest-bound, manager-owned cgroup
  identity and retryable cleanup boundary. It remains behind both references in
  live Linux, PTY, Windows and product-path evidence.

## Validation boundary

- The start test executes a real native child only to prove the previous bypass;
  the GREEN cgroup path stops before spawn because macOS has no cgroupfs.
- Ordinary directories prove ordering, rollback, idempotence and fd-relative
  authority, not Linux controller enforcement.
- Production selection still returns `linux_cgroup_v2_backend_not_wired`.
- The `Starting` crash windows, explicit cleanup journal, active schema 2
  replacement and live Linux pressure/recovery remain unverified.
