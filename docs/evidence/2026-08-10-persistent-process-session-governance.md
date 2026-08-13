# Persistent process session governance evidence — 2026-08-10

## Real lifecycle proof

- A real process with a 150 ms persisted deadline was terminated without a
  model poll. Its original PID disappeared and the manifest converged to
  `terminated / execution_deadline`.
- stdin activity extended the idle deadline; repeated read-only polls did not.
  This prevents observation from becoming an implicit keepalive.
- A separate owner test process created a live Tool process and exited through
  `std::process::exit(73)`. After the original absolute deadline, a replacement
  manager swept the persisted session, terminated the same PID and reported no
  ambiguous identity.
- A real loopback HTTP/SSE model started a process, delayed beyond the 100 ms
  policy, polled it and observed the typed deadline result in the next model
  turn. The Run completed with `governed process complete`, proving the policy
  is on the Agent Loop path rather than only a manager unit test.

## Multi-tenant and resource proof

- Cross-process admission rejected a second live session at the tenant limit
  even when it used another Workspace. A separate test rejected the second
  session at `(tenant, canonical Workspace)` scope.
- A noisy real process hit the configured hard output-file boundary and
  converged to `output_limit`; the boundary is `RLIMIT_FSIZE`, so it applies to
  all files written by that process and is not described as stdout-only.
- A child process observed the configured CPU limit. On macOS the default
  memory limit is absent and an explicit value is rejected. The non-macOS
  `RLIMIT_AS` path exists in source, but no Linux build or live host was used
  in this evidence run.
- Valid schema-1 terminal history migrated read-only. A live legacy session is
  not trusted as governed and therefore cannot be silently reattached.

## Failure and race proof

- Per-session sweep locking prevents concurrent background, interaction and
  replacement sweepers from publishing conflicting terminal state.
- SIGINT was repeated five times after the identity-transition fix; all runs
  reached a non-ambiguous terminal result. TERM-to-KILL process-group cleanup
  remains covered by the ADR-0070 tests.
- Governance is part of the Tool implementation digest, and the absolute
  deadline and limits are persisted before process publication. Replacement
  Hosts cannot substitute local defaults.

## Reference comparison

- Codex remains ahead in shipped `exec_command`/`write_stdin` UX and mature
  process-store integration. Its inspected store is process-local and may
  prune live entries at capacity; this Runtime instead rejects new work at
  explicit tenant/Workspace quotas and keeps existing ownership intact.
- OpenClaw remains ahead in PTY, resize, pause/resume, Node Host integration and
  cross-platform process adapters. Its overall/no-output timeout and bounded
  capture concepts informed this phase; this Runtime adds a persisted absolute
  deadline and replacement-Host governance binding.
- The stronger claims are limited to those two narrow multi-tenant recovery
  properties. They do not imply broader product maturity than either project.

## Validation boundary

- `cargo test --workspace --all-targets --quiet` completed with exit code 0.
  The authoritative listing contains 499 tests: 494 executed successfully and
  5 external live tests are explicitly ignored.
- The focused governance suites passed 8 manager cases, 1 separate-process
  crash/sweeper case and 3 standalone Agent Loop process-session cases.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.
- No Runtime test process or matching temporary directory remained. The Rust
  `target` build cache was preserved as required; Graphify output was absent.
- No external Provider key, Java, PostgreSQL, NATS, Docker or Kubernetes was
  used. Linux memory enforcement, Windows process governance and real vendor
  model behavior remain unverified.
