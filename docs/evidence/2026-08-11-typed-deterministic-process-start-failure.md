# Typed deterministic process-start failure evidence — 2026-08-11

## Behavioral RED/GREEN proof

- RED: the real synchronous spawn-failure test could only observe
  `ProcessSessionError::StartFailed` at the Manager boundary; the ToolExecutor
  had no corresponding typed variant.
- GREEN: the same test now executes through `ProcessSessionToolExecutor` while
  holding the real spawn boundary, waits for durable `launch_prepared`, removes
  the working directory and lets the OS reject spawn. Disk state is
  `Terminated/start_failed`, and the Tool caller receives
  `ProcessSessionStartFailed` with the same Session ID and a non-empty private
  reason.
- RED: the standalone Host returned `LocalRuntimeError::ToolExecution` and
  exposed the private reason instead of sending a Tool Result to the model.
- GREEN: a real loopback OpenAI-compatible HTTP Agent Loop receives
  `process_session_start_failed`, the safe fixed message and Session ID, then
  completes the Run successfully. The next Provider request and durable
  `tool.result` event contain the same JSON and neither contains the private
  reason.
- GREEN: the Worker classification test proves the same stable code, Session ID
  and redaction are used by its event-content helper.

The Host test substitutes the already-proven typed ToolExecutor result at the
executor boundary. This keeps the Agent Loop test deterministic on macOS, where
the actual program is launched behind the absolute `sandbox-exec` wrapper; the
real OS spawn failure itself remains independently covered at the Manager and
ToolExecutor boundary.

## Validation

- `agent-tool-runtime` passed all 84 tests under default parallelism.
- `cargo check --workspace --all-targets` passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed.
- `cargo fmt --all -- --check` passed.
- The first full-workspace run exposed a test-harness race: process-tree
  cancellation used a fixed two-second delay and could cancel before the real
  grandchild started under load. The test now observes the marker before
  cancelling, passed 10 consecutive focused runs, and still proves the marker
  stops advancing after cancellation.
- The final `cargo test --workspace --all-targets --quiet` run passed with zero
  failures. The inventory is 538 tests: 533 executed successfully and five
  external live cases explicitly ignored.

## Reference comparison

- Codex `ff352fab6209` propagates spawn I/O failure and remains stronger in
  parent-death handling, process groups, PTY, sandbox integration and product
  execution paths. The inspected process helpers do not provide this durable
  tenant/session replacement ledger.
- OpenClaw `58b4b9430457` records `spawn-error` in its supervisor and converts
  Tool exceptions into Agent Loop error results. It remains stronger in
  Unix/Windows adapters, timeout policy and PTY breadth, but the inspected path
  exposes raw exception messages and uses process-local supervision rather than
  this cross-Host durable certainty boundary.
- This Runtime is stronger only in the narrow combination of durable pre-spawn
  proof, stable Session identity and redacted model/event feedback. That does
  not imply overall process or Tool-runtime superiority.

## Validation boundary

- The model endpoint is a real local HTTP/SSE server, not an external vendor.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider or API key was used.
- Linux cgroup behavior remains ordinary-directory protocol evidence and
  fail-closed production code, not live kernel enforcement.
