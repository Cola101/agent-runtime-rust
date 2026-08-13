# ADR-0085: No retry after an accepted MCP Tool call

## Status

Accepted and implemented for the standalone Rust Host. Direct HTTP and cloud
gRPC Tool calls were already one-shot; their discovery operations may retry
under a separate bounded policy. The optional NATS Worker transport was not
started in this local stage.

## Context

The persistent stdio MCP client reused one reconnect loop for health checks,
`tools/list` and `tools/call`. A failed channel send is unambiguous: the actor
did not accept the request, so reconnecting is safe. A closed response channel
after a successful send is different: the actor accepted the request and the
remote Tool may already have produced an external side effect.

The old loop treated both cases alike. If the actor disappeared after applying
an Unknown-effect Tool but before returning its result, a replacement stdio
process received the same `tools/call` again.

## Decision

1. A failed queue send may reconnect because ownership of the complete unsent
   request is returned to the caller.
2. Health and `tools/list` may reconnect after response-channel loss because
   they are discovery/liveness operations and do not execute a Tool.
3. `tools/call` must never reconnect after its actor accepted the request. A
   lost response becomes `McpFederationError::Unreachable` with an explicitly
   unknown side-effect outcome.
4. The federated executor converts that transport failure into the shared Tool
   failure path. A default `ToolEffect::Unknown` makes the Run indeterminate;
   an operator-owned v18 override may classify failure differently, but never
   causes the transport to repeat the accepted call (ADR-0086).
5. Direct Streamable HTTP and cloud gRPC continue to issue one Tool request.
   Their catalog discovery may retry, but their Tool call may not.
6. A future idempotent MCP retry requires an operator-authoritative frozen
   effect policy and an idempotency contract. Server-supplied annotations alone
   cannot authorize replay or lower approval requirements.

## Consequences

### Positive

- Actor crashes, response truncation and transport loss converge on the same
  no-replay invariant after Tool acceptance.
- Safe discovery recovery remains available and is not conflated with Tool
  execution recovery.
- The local Runtime produces durable reconciliation evidence without Docker,
  Java, PostgreSQL, NATS or a cloud control plane.

### Negative and incomplete

- The Runtime remains conservative: absent an operator-owned v18 override, an
  MCP Tool freezes as Unknown even if its server advertises read-only or
  idempotent annotations.
- There is no cross-provider MCP idempotency-key standard in this implementation.
- The NATS publication branch and real Linux cgroup backend remain unverified
  against their external services.

## References

- Codex revision `ff352fab6209`,
  `codex-rs/rmcp-client/src/{rmcp_client,http_client_adapter}.rs`
- OpenClaw revision `58b4b9430457`,
  `src/agents/{agent-bundle-mcp-runtime,embedded-agent-subscribe.handlers.tools}.ts`
- `runtime/apps/runtime-host/src/stdio_mcp.rs`
- `runtime/apps/runtime-host/tests/standalone_run.rs`
- `docs/evidence/2026-08-11-mcp-accepted-call-response-loss.md`
