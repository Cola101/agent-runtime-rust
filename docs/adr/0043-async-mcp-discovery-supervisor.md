# ADR-0043: Async MCP discovery supervisor

## Status

Accepted. Implemented in the protocol-neutral Rust Runtime library and consumed
by the ADR-0044 single-writer coordinator; NATS adapter integration remains
pending.

## Context

ADR-0042 bounds and fairly schedules concurrent discovery requests, but the
existing Worker adapter still calls discovery inline after `accept` or
`restore`. One slow MCP server therefore pauses the entire assignment poll even
though the network layer itself can run many bounded requests.

Moving `WorkerProcessor` into background tasks is not acceptable: network
completion order would then decide Kernel mutation order, event publication and
Checkpoint timing. The concurrency boundary must surround only external I/O.

Codex concurrently resolves one session's MCP catalog and has much richer
connection lifecycle/cache behavior, but does not expose a cross-Run result
supervisor. OpenClaw's process command queue tracks active tasks, timeouts,
pause and drain across lanes; that lifecycle is a useful reference, while its
tasks own their command execution rather than returning a Kernel-neutral
catalog result.

## Decision

1. `McpDiscoverySupervisor` owns only spawned network tasks, a bounded result
   channel and an in-process set of active attempt IDs. It has no NATS,
   database, Java or container dependency.
2. Attempt identity is derived from `RunExecutionCommand`; callers cannot label
   a command as another attempt. A duplicate active attempt is refused.
3. Each task receives cloned immutable command/registry state, a shared gateway
   client and the attempt cancellation token. Cancellation wins a simultaneous
   race and drops the discovery future, releasing ADR-0042 admission permits.
4. A task emits exactly one terminal `Ready` or `Cancelled` update. `Ready`
   contains `FederatedRunTools` but never mutates `WorkerProcessor`.
5. The ADR-0044 coordinator preserves the Host as single writer: it receives an
   update, attaches the catalog, starts a new Run or verifies recovery bindings;
   the Host then emits events/Checkpoint and resumes model work in its serial
   loop.

## Consequences and limits

- A slow Run no longer has to serialize another Run's MCP network work.
- Network completion order cannot directly reorder Kernel state transitions.
- Supervisor state is intentionally ephemeral. A process crash relies on the
  Host's durable command/checkpoint mechanism to relaunch discovery.
- The NATS adapter still calls discovery inline. Until it consumes supervisor
  updates for both admission and recovery, production Worker throughput and
  message-ack behavior are not proven by this ADR.
- Drain observability, forced shutdown of all active discoveries and a durable
  per-attempt admission ledger remain future lifecycle work.

## References

- ADR-0040 MCP tool federation
- ADR-0042 shared MCP discovery admission
- ADR-0044 single-writer MCP discovery coordinator
- Codex `codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs`
- OpenClaw `src/process/command-queue.ts`
