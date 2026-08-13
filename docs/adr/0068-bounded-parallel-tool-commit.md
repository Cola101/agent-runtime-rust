# ADR-0068: Bounded parallel Tool execution with deterministic commits

## Status

Accepted and implemented in the standalone Rust Runtime. Real loopback model
traffic and real child Tool processes prove bounded overlap, source-order
transcript commits and replacement-Host recovery from a half-finished batch.

## Context

One assistant turn may request several independent Tools. The Runtime formerly
planned and executed every Tool serially, leaving substantial latency on
independent reads. Naively spawning all calls is unsafe: approval, side effects,
Workspace writes, cancellation and Checkpoint recovery can change observable
order or duplicate work.

Codex uses a read/write execution gate: Tools that explicitly support parallel
calls share the read side and serial Tools take the write side. OpenClaw first
performs sequential preflight, runs a batch concurrently only when no call is
marked sequential, then emits Tool result messages in source order. Both
patterns require a deliberate admission decision rather than transport-order
fan-out.

## Decision

1. RunExecution schema 17 freezes runtime-policy schema 4 and a
   `max_concurrent_tools` value in the range 1–16. The default is four; older
   policy schemas are permanently serial.
2. Planning, delegated-scope checks and approval remain sequential. Only an
   adjacent prefix of `ToolEffect::Pure` calls may overlap. Idempotent,
   non-idempotent, unknown, denied, approval-gated, built-in subagent and
   federated MCP calls are serial barriers for this milestone.
3. The Worker freezes the admitted requests in the assistant's original Tool
   Call order. A reordered transport list, catalog mismatch or batch above the
   frozen limit fails closed.
4. Every `tool.execution.started` event and Checkpoint is durable before a
   child process starts. Completed results may arrive in any order, but are
   staged until a contiguous source-order prefix can be committed to Kernel
   events and the model transcript.
5. Worker Checkpoint schema 24 persists the commit queue and staged results.
   Recovery retries only unfinished Pure calls and releases already staged
   results in their original order. At this milestone, started non-idempotent
   or unknown calls retained the existing ambiguous-side-effect refusal;
   ADR-0069 now supersedes that boundary with an explicit terminal state and
   reconciliation contract.
6. The standalone Host uses a bounded asynchronous completion set. Cancellation
   drains and reaps owned work before producing the terminal state. The optional
   NATS adapter uses the same ordered staging API and does not advance the model
   while a batch remains active.

## Consequences

### Positive

- Independent reads reduce wall time without changing Tool Result order.
- Placement, restart and completion timing cannot change the transcript seen by
  the next model turn.
- The concurrency limit and recovery semantics are part of the signed execution
  contract instead of a Host-local tuning flag.

### Negative and incomplete

- The current admission rule is intentionally narrower than Codex: a Tool
  cannot yet declare a reviewed parallel capability independent of `Pure`.
- OpenClaw can concurrently run broader batches after execution-mode preflight;
  this Runtime keeps all side-effecting and federated calls serial until
  conflict keys and replay receipts exist.
- The NATS path compiles and shares ordered commit state, but this milestone did
  not start an external NATS service or prove its PubAck crash windows.
- ADR-0069 now converges a started ambiguous side effect to a stable
  `indeterminate` terminal state and supports versioned operator reconciliation.
  The NATS adapter still lacks an external broker crash-window run.

## References

- Codex `codex-rs/core/src/tools/parallel.rs`
- Codex `codex-rs/core/src/tools/router.rs`
- OpenClaw `packages/agent-core/src/agent-loop.ts`
- `contracts/events/run-execution-requested.v17.example.json`
- `runtime/crates/protocol/src/lib.rs`
- `runtime/apps/worker/src/lib.rs`
- `runtime/apps/runtime-host/src/lib.rs`
