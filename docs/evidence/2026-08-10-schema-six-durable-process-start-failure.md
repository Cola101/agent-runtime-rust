# Schema-six durable process start failure evidence — 2026-08-10

Update 2026-08-11: ADR-0083 closes the ToolExecutor/Worker/Host propagation
gap recorded below; this file remains the evidence for the Manager/schema-6
stage itself.

## Behavioral RED/GREEN proof

- RED: after a real synchronous `Command::spawn` failure, the Manifest remained
  `Starting/prepared`. The test holds the real spawn boundary, waits for the
  durable prepared record, removes the command working directory, and releases
  the boundary so the operating system returns the failure.
- GREEN: the same path now persists `Terminated/cleaned`, operation sequence 3,
  `last_operation=start_failed`, and `termination_reason=start_failed`; the
  direct caller receives `ProcessSessionError::StartFailed` with the same
  Session ID and a non-empty reason.
- RED: a live schema-5 Unix session reattached successfully but remained schema
  5, which would allow a new enum value under an old version.
- GREEN: the replacement Manager digest-verifies the schema-5 record,
  reattaches the original PID/PGID, rewrites schema 6, and closes the original
  process without restarting it.

## Validation

- `agent-tool-runtime` passed 84 tests under default parallelism.
- `cargo check --workspace --all-targets` passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo fmt --all -- --check` passed.
- The first full-workspace run exposed an existing test-harness defect:
  `subagent_approval` assumed one TCP read contained a full HTTP request. Its
  provider now reads headers plus the complete `Content-Length` body; the
  focused real daemon/child approval test passed afterward.
- The final `cargo test --workspace --all-targets --quiet` run passed with zero
  failures. The inventory is 536 tests: 531 executed successfully and five
  external live cases explicitly ignored.

## Reference comparison

- Codex `ff352fab6209` propagates spawn I/O failure directly and has stronger
  parent-death, process-group, PTY and sandbox product paths. The inspected path
  does not provide this multi-tenant replacement-Host Manifest.
- OpenClaw `58b4b9430457` records `spawn-error` in its in-process supervisor and
  remains stronger in Unix/Windows tree termination, timeout arbitration and
  adapters. The inspected registry is not a durable cross-Host journal.
- This Runtime is stricter only at the durable recovery boundary: terminal
  launch truth is persisted before cleanup and old active state is migrated
  without replay. ToolExecutor/Worker typed propagation remains incomplete.

## Validation boundary

- Spawn failure and schema-5 reattachment use real native processes and
  PID/PGID/identity locks on macOS; the model is not involved in this error-path
  proof.
- cgroup cases remain ordinary-directory protocol fixtures, not real Linux
  enforcement.
- Graphify mapped 444 nodes and 1,542 post-build edges across 17 communities.
  Its post-build health check found zero missing, dangling, self-loop or
  collapsed edges; it cannot prove whether producer-side suppression happened,
  so source and runtime behavior remain authoritative.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider or API key was used.
