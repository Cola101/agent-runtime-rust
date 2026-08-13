# Linux cgroup v2 protocol boundary evidence — 2026-08-10

## TDD proof

- The first compile failed because the cgroup operations and backend selector
  did not exist. Minimal stubs then produced four behavioral failures: no
  controller values were written, a symlink was accepted, membership remained
  empty and CPU usage returned zero.
- The completed protocol passes all 4 focused unit cases. It writes the five
  configured controller values, rejects a final-component symlink without
  touching the target, writes `0\n` to membership and strictly parses one
  `usage_usec` value.
- The capability integration suite passes 5/5. On this macOS host, selecting
  Linux cgroup v2 returns `UnsupportedPlatform` before the configured state
  root exists. Existing unsupported memory, PID and whole-tree requirements
  retain the same fail-closed behavior.

## Agent Runtime closure

- Existing process governance tests pass 7/7.
- The standalone Host process loop passes 3/3 against real child processes and
  loopback HTTP/SSE model turns: normal lifecycle, replacement continuation and
  model-visible deadline termination remain intact.
- Linux activation is intentionally absent. The protocol is not called by a
  production session until Manifest identity, pre-exec membership, CPU
  supervision, cgroup kill/cleanup and recovery are connected atomically.

## Reference comparison

- Codex revision `ff352fab6209` remains ahead in interactive process UX, PTY
  and process-store integration. The inspected `ProcessStore` is process-local
  and can prune an older live entry; it is not a persisted tenant cgroup ledger.
- OpenClaw revision `58b4b9430457` remains ahead in overall/no-output timeout,
  TERM-to-KILL escalation, PTY and cross-platform adapters. Its inspected
  supervisor registry is process-local; cgroup readings elsewhere are
  diagnostic rather than per-Tool admission.
- This Runtime is stricter only at the narrow activation boundary: an
  incomplete backend cannot advertise or accept guarantees it does not yet
  enforce. That is not overall process-subsystem superiority.

## Validation and instability disclosure

- The first full workspace run exposed one timeout in
  `a_new_host_recovers_the_same_async_handle_without_replaying_spawn`: the
  loopback Provider did not observe both old sockets closing within five
  seconds. The exact test then passed 1/1 and the complete subagent concurrency
  suite passed 20/20 without a code or timeout change.
- A clean second `cargo test --workspace --all-targets --quiet` run exited 0:
  502 tests passed, 0 failed and 5 external live tests were explicitly ignored,
  for 507 total. The earlier timeout remains a stabilization risk and is not
  reclassified as proven fixed by the successful rerun.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` pass.
- Only macOS ARM64 was available. No Linux target, cgroupfs, external Provider,
  Docker, Java, PostgreSQL, NATS or Kubernetes was used.

## Residual risk

- Ancestor directory replacement remains unresolved; final controller files
  are protected by validation plus `O_NOFOLLOW`, but delegated-root containment
  needs fd-relative resolution.
- `cgroup.procs`, `cpu.stat` and controller writes have no real kernel proof.
- Recovery cannot yet rediscover and fence a cgroup, and terminal cleanup does
  not yet use `cgroup.kill`.
- The subagent recovery socket-close timeout needs a deterministic crash-harness
  or lifecycle fix before it can be considered closed.

## Follow-up

ADR-0074 subsequently reproduced the timeout deterministically while the Tokio
Runtime remained alive and fixed it with a Host-owned cancellation domain. The
original observation above remains part of this stage's evidence; its current
resolution is documented in
`docs/evidence/2026-08-10-host-owned-cancellation-domain.md`.
