# FD-relative cgroup lifecycle evidence — 2026-08-10

## TDD proof

- The delegated-root replacement test first produced a behavioral RED: after
  the configured path was renamed and replaced, path-based configuration wrote
  the replacement `memory.max` while the originally opened root stayed empty.
- Two group-replacement tests first failed by reading replacement CPU state and
  by leaving the original `cgroup.procs` unchanged. The implemented `openat`
  path now reads, kills and installs pre-exec membership through the original
  group descriptor.
- The preparation test first returned `GroupAlreadyExists` from an attacker
  group under the replacement path. `mkdirat` plus descriptor-relative
  `unlinkat(AT_REMOVEDIR)` now creates and rolls back only beneath the opened
  original root, without touching the replacement sentinel.
- The old PathBuf lifecycle functions were removed after all tests migrated to
  the descriptor API.

## Regression found by the gate

- The first package-wide run exposed one existing concurrent
  `process_tree_reaping` failure. A focused run passed, showing a timing window
  rather than a deterministic cgroup regression.
- Tool spawn already enforces `process_group(0)`, so child PID is the process
  group ID. Re-resolving it with `getpgid` during timeout created an avoidable
  leader-exit/PID-reuse window. Reaping now signals that known group ID
  directly, matching the spawn invariant.
- The real child/grandchild timeout and cancellation tests then passed 10
  consecutive runs, 20/20 cases, and the package default-parallel gate passed
  all 68 tests.

## Full validation

- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo test --workspace --all-targets --quiet` passed under default
  parallelism. The test inventory contains 520 tests: 515 executed and passed,
  zero failed, and five external live tests remained explicitly ignored.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, virtual machine, external
  Provider or API key was used.

## Reference comparison

- Codex `ff352fab6209` has mature process-group helpers, Linux parent-death
  signaling, PTY integration and sandbox product paths. The inspected Tool
  execution path does not provide this persisted tenant/session cgroup ledger.
- OpenClaw `58b4b9430457` has a broader Unix/Windows tree-kill implementation,
  process-group leader verification and a process supervisor with overall and
  no-output timeouts. The inspected supervisor remains process-local rather
  than a replacement-Host cgroup identity.
- This Runtime is stricter only in the narrow fd-relative, digest-bound
  resource-identity boundary. It is still behind both references in live
  product maturity, PTY breadth and cross-platform execution.

## Validation boundary

- The containment tests use ordinary directories/files on macOS. They prove
  descriptor anchoring, final-component symlink rejection, exact bytes and
  pre-exec ordering; they do not prove Linux cgroup enforcement.
- Production selection still returns `linux_cgroup_v2_backend_not_wired`.
- Manager-lifetime root pinning, terminal empty-group removal, active schema 2
  replacement and live Linux memory/PID/CPU/kill pressure remain unverified.
