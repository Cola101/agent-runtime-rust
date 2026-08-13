# Effect-aware live Tool failure evidence — 2026-08-11

## Behavioral RED/GREEN proof

- Worker RED: no API existed that could receive an executor error while still
  consulting the frozen Tool effect. `poll_tool_once` unconditionally called
  `record_bound_tool_completion`, so every live failure became `tool.result`.
- Worker GREEN: real Worker state is driven through model Tool Call, completed
  model turn and durable `tool.execution.started`. Generic failures of
  `NonIdempotent` and `Unknown` now produce `run.indeterminate`; the same error
  for `Pure` and `Idempotent` produces a redacted error Tool Result. A typed
  process-start failure remains a safe Tool Result even when its Tool is
  non-idempotent because the process is proven not to have started.
- Host RED: a real loopback HTTP Agent Loop executed a non-idempotent Tool that
  wrote a marker and then returned a private engine error. The Host aborted with
  `LocalRuntimeError::ToolExecution` and exposed the private reason to its caller.
- Host GREEN: the same Agent Loop writes the marker exactly once, emits and
  checkpoints `run.indeterminate`, and returns a normal indeterminate outcome.
  An operator `Applied` decision starts a separate continuation Run; the model
  receives the reconciliation Tool Result, the continuation succeeds, and the
  original Tool is not called again. Durable events contain no private reason.

## Validation

- The three new Worker assignment cases pass with the full assignment suite.
- Both focused Host failure Agent Loops pass: deterministic process-start error
  continues in the same Run; ambiguous live side effect stops for reconciliation.
- The first default-parallel workspace run exposed a restart-harness race: a
  deliberately aborted Host could leave a zero-byte Provider connection. A
  behavioral guard reproduced it; the Provider now skips only a completely
  empty abandoned connection and still rejects partial HTTP. The original
  restart case then passed 10 consecutive focused runs.
- Full workspace test, check, Clippy and formatting results are recorded in the
  current implementation status after the final gate.

## Reference comparison

- Codex has mature Tool lifecycle events, parallel cancellation, sandbox errors
  and model-visible failure outputs. In the inspected path, generic Tool failure
  becomes a response item without a persisted per-call effect certainty ledger.
- OpenClaw distinguishes preflight from `executionStarted`, provides strong
  process timeout/PTY adapters and warns that timeout side effects may already
  have completed. Its inspected Agent Loop still returns the thrown exception as
  an error Tool Result and does not make the Run indeterminate by Tool effect.
- This Runtime is stronger only in the narrow multi-tenant recovery invariant:
  frozen effect plus durable started identity decides whether a live failure may
  become a Tool Result. Codex/OpenClaw remain broader product runtimes.

## Validation boundary

- The model endpoint is a real local HTTP/SSE server and the Tool performs a
  real filesystem side effect before failing.
- No Docker, virtual machine, Java, PostgreSQL, NATS, Kubernetes, external
  Provider or API key was used.
- NATS event publication, real remote MCP response loss and Linux cgroup kernel
  behavior remain explicitly unverified in this stage.
