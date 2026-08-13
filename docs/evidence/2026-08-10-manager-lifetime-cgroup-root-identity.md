# Manager-lifetime cgroup root identity evidence — 2026-08-10

## TDD proof

- The first RED was a compile failure because no resolved runtime backend type
  existed. This proved the manager API exposed only the pathname configuration.
- A temporary path-backed implementation then produced the behavioral RED: the
  test renamed the original delegated root, created a replacement hierarchy and
  observed `usage_usec = 7` instead of the original root's `2,000,000`.
- The implemented manager opens `LinuxCgroupV2Root` once, stores it in an `Arc`
  and propagates clones into foreground operations, the watcher and governance
  supervisor. The same test now observes `2,000,000`; the replacement sentinel
  is not treated as manager authority.
- The existing CPU observation test was migrated to the resolved backend so no
  operational helper can silently reopen a configured root path.

## Full validation

- The `agent-tool-runtime` package passed all 69 tests under default
  parallelism, and its all-features Clippy gate passed with warnings denied.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo test --workspace --all-targets --quiet` passed under default
  parallelism. The inventory contains 521 tests: 516 executed and passed, zero
  failed, and five external live tests remained explicitly ignored.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, virtual machine, external
  Provider or API key was used.

## Reference comparison

- Codex `ff352fab6209` has mature process-group helpers, Linux parent-death
  signaling, PTY integration and sandbox product paths. The inspected process
  helpers do not provide a persisted tenant/session manager-held cgroup root.
- OpenClaw `58b4b9430457` has broader Unix/Windows tree kill, process timeouts
  and adapter supervision. Its inspected supervisor registry is process-local
  and does not provide this replacement-Host cgroup authority.
- This Runtime is stricter only in the narrow manager-lifetime, digest-bound
  cgroup identity boundary. It remains behind both references in live Linux,
  PTY, Windows and product-path evidence.

## Validation boundary

- The replacement test uses ordinary directories/files on macOS. It proves
  stable descriptor identity across manager operations, not Linux cgroup
  enforcement.
- Production selection still returns `linux_cgroup_v2_backend_not_wired`.
- Start/terminal lifecycle integration, active schema 2 replacement and live
  Linux memory/PID/CPU/kill/cleanup remain unverified.
