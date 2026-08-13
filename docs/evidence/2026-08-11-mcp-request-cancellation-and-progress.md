# MCP request cancellation and progress evidence — 2026-08-11

## Behavioral RED/GREEN proof

- RED: a real Streamable HTTP MCP server sent two progress notifications and
  kept `tools/call` open. Cancelling the Run only closed the original socket;
  no `notifications/cancelled` arrived and no progress event was durable.
- GREEN: the request carries a non-empty progress token; both matching updates
  enter the Run's monotonic sequence and Checkpoint. Cancellation sends a
  sibling, session-aware notification whose `requestId` matches the live call,
  then closes the original response stream.
- A real stdio fixture independently emits progress, observes the matching
  cancellation notification on stdin, exits cooperatively, and leaves no
  process tree behind.
- Existing discovery cancellation remains immediate. The regression gate
  proves a stalled `tools/list` still tears down the complete stdio process
  group rather than waiting for the ordinary request timeout.

## Runtime semantics

- The Tool start remains durable before remote execution.
- Each progress event carries `tool_call_id`, `binding_digest`, progress,
  optional total and a bounded message; the event is persisted before another
  update is consumed.
- Cancellation after an Unknown MCP Tool started still produces a durable
  `run.indeterminate`. Protocol cancellation is not treated as evidence that
  the external side effect was rolled back.
- Progress queues are bounded and non-blocking. Losing an intermediate update
  does not lose the final Tool result or stall the Agent Loop.

## Reference comparison

- Codex has the broader `rmcp` transport, request metadata and notification
  handlers. In the inspected Tool-call path, progress notifications are logged
  but were not found bridged into a durable Run event/Checkpoint; an explicit
  cancellation notification from the Tool-call abort path was not located.
- OpenClaw binds HTTP disconnect to an `AbortController`, but its loopback MCP
  handler explicitly treats `notifications/cancelled` as a no-op and no
  `progressToken` path was found in the inspected gateway implementation.
- The Rust Runtime is ahead only in this narrow, tested lifecycle-to-durability
  bridge. Codex and OpenClaw remain ahead in MCP breadth, OAuth/Apps, UI and
  cross-platform product integration.

## Validation boundary

- Exercised local TCP, SSE streaming, JSON-RPC request identity, local stdio,
  real process cleanup, Kernel events, on-disk event replay and Checkpoint.
- No Docker, Java, PostgreSQL, NATS, Kubernetes, VM, external Provider or API
  key was used.
- Cloud gRPC lifecycle streaming, real external MCP servers and MCP Tasks are
  explicitly unverified.
