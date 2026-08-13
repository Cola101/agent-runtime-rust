# Explicit process resource capability evidence — 2026-08-10

## TDD proof

- The first test compile failed because the capability types, governance fields
  and typed error did not exist. A minimal API scaffold then produced the real
  behavioral RED: macOS accepted an unenforceable memory requirement, and the
  manager accepted process-count and whole-tree requirements.
- After capability validation was added, all 4 focused cases passed. The two
  unsupported policies fail before the configured state root exists, proving
  the rejection occurs before durable state or process launch.
- macOS reports the literal vector `UnixRlimit + output-file + CPU-time`, with
  memory, process count and whole-tree accounting all false.

## Agent Runtime closure

- The existing standalone Host process-session suite passed 3/3 after the
  capability contract was introduced. A real loopback HTTP/SSE model still
  exercises start/write/poll/close, replacement-Host continuation and model-
  visible deadline termination against real child processes.
- The capability fields are not Tool arguments and cannot be chosen by the
  model. They enter the manager governance digest, which already enters all five
  process Tool implementation digests and therefore the normal policy,
  approval, durable-start and Checkpoint path.

## Reference comparison

- Codex revision `ff352fab6209` remains ahead in interactive process UX and
  process-store integration. Its inspected capacity/pruning path is not a
  persisted tenant resource capability contract.
- OpenClaw revision `58b4b9430457` remains ahead in PTY, Node Host and
  cross-platform adapters. Its process supervisor supplies overall/no-output
  timeout and TERM-to-KILL; cgroup reads elsewhere are diagnostic and do not
  prove per-Tool cgroup admission.
- The new contract is therefore a prerequisite, not a claim that this Runtime
  has surpassed either project's process subsystem.

## Validation boundary

- `cargo test --workspace --all-targets --quiet` exited 0. The authoritative
  listing contains 502 tests: 497 executed successfully and 5 external live
  tests are explicitly ignored.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.
- No matching Runtime process or temporary test directory remained. The
  temporary Graphify graph and test log were removed; Rust `target` was kept.
- Graphify found the expected bridge through `PersistentProcessSessionManager`
  and `validate_governance`, but reported 75 dangling edges and 104 directed
  same-endpoint collapses. It was used only for routing; source and tests remain
  authoritative.
- Only macOS ARM64 was available. No Linux target, Linux build, cgroup live
  enforcement, external Provider key, Java, PostgreSQL, NATS, Docker or
  Kubernetes was used.
