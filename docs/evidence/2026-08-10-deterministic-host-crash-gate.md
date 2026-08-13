# Deterministic Host crash gate evidence — 2026-08-10

## Observed failure

- A default-parallel workspace run left
  `a_new_host_recovers_the_same_async_handle_without_replaying_spawn` running
  for several minutes. The same test passed alone in 0.18 seconds and the
  serial workspace gate passed.
- Repeating the 20-test `subagent_concurrency` binary reproduced the hang on
  the second run. Adding bounded phase assertions converted it into a stable
  failure instead of an unbounded quality gate.

## Root cause

- The crash driver waited only for the parent active-handle Checkpoint and the
  child Checkpoint. Those files can become durable before the deterministic
  Provider has accepted both post-spawn model connections.
- The driver could therefore abort the Host while the Provider was still
  blocked in `listener.accept()`. The Provider never reached its crash-signal
  receiver, so the test incorrectly reported that Host Drop retained sockets.
- This was a test synchronization defect. No production connection leak was
  proven, and no production lifecycle code was changed.

## Fix and TDD proof

- The Provider now emits an explicit one-shot readiness edge only after it has
  accepted and parsed both the parent and child connections. The crash driver
  requires both durable Checkpoints and that real connection edge before
  aborting the Host.
- Crash-signal delivery, Provider acknowledgement, socket closure, replacement
  Run completion, Provider completion and Host shutdown all have explicit
  deadlines. A missing request now fails with the exact phase instead of
  hanging the workspace gate.
- Before the readiness edge, the bounded test repeatedly failed with
  `Provider did not observe the simulated crash`. After the fix, 20 consecutive
  default-parallel runs passed all 20 cases: 400/400.

## Full validation

- `cargo check --workspace --all-targets` passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- Default-parallel `cargo test --workspace --all-targets --quiet` passed 511
  tests with zero failures; five external live tests remained explicitly
  ignored. The suite still contains 516 tests.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, virtual machine or external
  Provider was started, and no test process or temporary state remained.

## Reference boundary

- Codex revision `ff352fab6209` uses bounded real background-child/process
  group tests in `codex-rs/core/src/exec_tests.rs` and
  `codex-rs/utils/pty/src/tests.rs`.
- OpenClaw revision `58b4b9430457` uses explicit supervisor deadlines and
  deterministic adapter settlement in `src/process/supervisor/supervisor.test.ts`.
- This change aligns the evidence discipline only. It does not close the
  remaining Codex PTY/sandbox breadth or OpenClaw cross-platform supervisor
  gap.
