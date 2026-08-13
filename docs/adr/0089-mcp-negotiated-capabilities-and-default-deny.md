# ADR-0089: MCP negotiated capabilities and default-deny reverse requests

## Status

Accepted and implemented for the standalone Rust Host's Streamable HTTP and
stdio transports. No MCP client-side reverse capability is enabled yet.

## Context

MCP is bidirectional. A server may request client work such as
`sampling/createMessage`, `elicitation/create`, or `roots/list`, but only after
the client advertised the corresponding capability during initialization.
Allowing one of these requests to reach the model, workspace, approval system,
or budget without an explicit grant would bypass the Agent's frozen authority.

The Host already advertised an empty client capability set, but its behavior
was incomplete: stdio returned JSON-RPC `-32601` and then continued accepting a
Tool result, while Streamable HTTP ignored server requests carried by SSE. The
handshake also accepted any selected protocol version and did not require the
server's `tools` capability before `tools/list` or `tools/call`.

Resources and Prompts are not reverse requests. They are server capabilities
queried by the client and remain a separate breadth gap.

## Decision

1. The current client continues to advertise `{}` capabilities. Sampling,
   elicitation, and roots are therefore unavailable by construction.
2. Initialization accepts only protocol `2025-06-18` and requires the server to
   advertise an object-valued `capabilities.tools` before any Tool traffic.
   Every JSON-RPC response must echo the exact active request ID.
3. A JSON-RPC request from the server with both `method` and `id` is treated as
   an unnegotiated authority request. HTTP and stdio echo the exact ID with
   error code `-32601` and then fail/retire the session.
4. Streamable HTTP parses SSE incrementally during initialization, discovery,
   and Tool execution so it can answer a reverse request without waiting for
   the original response stream to close. The rejection is a session-aware
   sibling POST.
5. stdio grants the peer at most 100 ms to consume the flushed rejection, then
   discards any later frame and reaps the process group.
6. A violation during required discovery fails before model egress. A violation
   after an Unknown or NonIdempotent Tool start preserves the existing durable
   `run.indeterminate` rule; a later successful Tool result is not trusted.
7. No reverse request can invoke a Provider, create an Approval, consume a Run
   budget, access Workspace roots, or write a Checkpoint except for the
   protocol-violation terminal evidence already owned by the Host.

## Consequences and limits

- The current narrow MCP Tool client is now honest about its negotiated surface
  and fails closed on a server that exceeds it.
- This is not support for sampling, elicitation, or roots. Enabling any one of
  them requires a frozen per-Run grant, delegated scope, budget accounting,
  approval semantics, durable request identity, and recovery behavior.
- Resources and Prompts remain unimplemented client-initiated features.
- The optional cloud gRPC MCP path still lacks this bidirectional lifecycle
  contract and is not covered by this decision.

## References

- [MCP lifecycle and capability negotiation](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)
- [MCP architecture](https://modelcontextprotocol.io/specification/2025-06-18/architecture)
- Codex revision `ff352fab6209`, `codex-rs/codex-mcp/src/rmcp_client.rs` and
  `codex-rs/rmcp-client/src/elicitation_client_service.rs`
- OpenClaw revision `58b4b9430457`, `src/gateway/mcp-http.handlers.ts`
- `runtime/apps/model-gateway/tests/mcp_federation.rs`
- `runtime/apps/runtime-host/tests/execution_cancellation.rs`
- `docs/evidence/2026-08-11-mcp-negotiated-capabilities-and-default-deny.md`
