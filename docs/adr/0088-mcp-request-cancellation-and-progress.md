# ADR-0088: MCP request cancellation and durable progress

## Status

Accepted and implemented for the standalone Rust Host's Streamable HTTP and
stdio transports. The cloud gRPC federation adapter can cancel its unary call,
but does not yet transport MCP progress or a protocol cancellation notification
through the Gateway boundary.

## Context

ADR-0087 made interruption honest about uncertain side effects, but transport
cleanup was still only connection closure or process-group termination. An MCP
server could not distinguish caller cancellation from network loss, and its
progress notifications disappeared before the Kernel event log.

The Runtime declares MCP protocol version `2025-06-18`. That version defines a
request `_meta.progressToken`, `notifications/progress`, and
`notifications/cancelled` bound to the original JSON-RPC request ID.

## Decision

1. Every active `tools/call` receives a token derived from the immutable
   attempt and Tool call identity. The same token is unique among active calls.
2. HTTP consumes SSE incrementally. stdio consumes JSONL notifications on the
   existing session. Only matching progress tokens are accepted.
3. Progress must be finite and strictly increasing. Optional totals must be
   finite and messages are limited to 2048 bytes.
4. Transport and Tool-executor queues are bounded to 32 entries and use
   non-blocking delivery. Intermediate progress may be dropped under pressure;
   execution and the final Tool result cannot be blocked by a noisy server.
5. Accepted progress becomes a monotonic `tool.execution.progress` Run event
   containing the Tool call and binding digest. The standalone Host persists a
   Checkpoint after every emitted progress event.
6. Cancellation sends `notifications/cancelled` with the exact active request
   ID and a bounded operator reason. HTTP uses a session-aware sibling POST;
   stdio writes on the same channel and grants a short cooperative shutdown
   window before process-group TERM/KILL cleanup.
7. Cancellation remains advisory at the MCP layer. A started Unknown or
   NonIdempotent Tool still terminates as `run.indeterminate`; neither a sent
   notification nor a closed transport proves whether the side effect happened.
8. Progress is observational, not a replay checkpoint. Recovery never resumes
   a Tool from a percentage and never treats progress as a Tool result.

## Consequences and limits

- The standalone HTTP and stdio paths now express normal MCP request lifecycle
  instead of making disconnect the only cancellation signal.
- Progress is durable and reconnectable through the existing event log without
  allowing remote backpressure to control the Runtime.
- The currently declared protocol does not use the newer task-augmented
  `tasks/cancel` flow. MCP Tasks remain unimplemented.
- The optional cloud gRPC path needs a streaming lifecycle contract before it
  can make the same protocol-level guarantee; compilation is not live proof.
- Resources, Prompts, sampling, elicitation, OAuth and MCP Apps remain outside
  this decision.

## References

- [MCP cancellation utility](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation)
- [MCP progress utility](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/progress)
- Codex revision `ff352fab6209`, `codex-rs/rmcp-client` and
  `codex-rs/codex-mcp/src/connection_manager.rs`
- OpenClaw revision `58b4b9430457`, `src/gateway/mcp-http.ts` and
  `src/gateway/mcp-http.handlers.ts`
- `runtime/apps/runtime-host/tests/execution_cancellation.rs`
- `docs/evidence/2026-08-11-mcp-request-cancellation-and-progress.md`
