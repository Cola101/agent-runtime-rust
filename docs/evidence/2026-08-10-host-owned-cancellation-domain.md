# Host-owned cancellation domain evidence — 2026-08-10

## TDD proof

- The recovery test was strengthened before production code changed. It kept
  Tokio alive, aborted and awaited the first Host execution, then required the
  loopback Provider to observe both real TCP sockets close.
- RED was deterministic: after five seconds the Provider reported
  `crashed runtime left provider sockets open`, and the test reported
  `aborted Host retained a Provider connection`.
- Adding the Host-owned child cancellation domain plus Drop cancellation/abort
  made the same test pass in 0.18 seconds without increasing any timeout.
- The production mutation that removes Host Drop cancellation makes this test
  fail for the original connection leak, so the assertion is not a source-text
  or mock check.

## Real closure

- The Provider is a real loopback TCP HTTP/SSE server. It accepts the parent
  model turn and asynchronous child turn, then reads both connections until EOF
  or error before allowing replacement recovery.
- Only after both old connections close does a replacement Host resume the
  same durable `agent_id`. The final outcome succeeds and emits zero new
  `subagent.spawn.requested` events, proving cancellation did not cause spawn
  replay.
- Removing the nested Tokio Runtime from the harness eliminated an unrelated
  unbounded Runtime-drop wait under concurrent test binaries. The direct Host
  ownership assertion remains unchanged.

## Regression gates

- Exact recovery case: 1/1 passed.
- `execution_cancellation`, `daemon_recovery`, `standalone_run` and
  `subagent_concurrency` together: 62/62 passed; the complete subagent suite
  finished in 1.78 seconds.
- `cargo test --workspace --all-targets --quiet`: 502 passed, 0 failed and 5
  external live cases explicitly ignored, 507 total.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.

## Reference comparison

- Codex `ResponseStream::Drop` explicitly cancels the upstream mapper when a
  consumer disappears, and session shutdown explicitly terminates processes.
  Codex still has broader turn, PTY and interactive process lifecycle coverage.
- OpenClaw's supervisor explicitly kills and disposes adapters and escalates
  TERM to KILL. It remains broader in process/PTY/platform adapters.
- This Runtime now adds durable replacement proof across parent and subagent
  model connections. That is a narrower multi-tenant recovery property, not a
  claim of overall lifecycle superiority.

## Validation boundary

- Only local macOS loopback connections were used. No external Provider,
  Docker, Java, PostgreSQL, NATS or Kubernetes was started.
- OS process death, Linux cgroup recovery and remote MCP connection ownership
  remain separate gates.
